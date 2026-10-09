//! Who a relay of the server's own lets through (P6-04).
//!
//! The relay runs as a process of its own (`spoolframe-server relay`),
//! with no users of its own: it asks the main server, over an Iroh
//! connection on [`RELAY_ACCESS_ALPN`], which tier an endpoint that
//! connects to it is in. The main server answers only the relays its
//! config names. One bidirectional stream per question: an
//! [`AccessQuery`] frame, answered by an [`AccessAnswer`] frame.

use serde::{Deserialize, Serialize};

/// The ALPN a relay asks the main server on.
pub const RELAY_ACCESS_ALPN: &[u8] = b"spoolframe/relay-access/1";

/// Which tier `endpoint` is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessQuery {
    /// The endpoint's key, hex.
    pub endpoint: String,
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
    /// The server itself, or a device of its users: one with a live
    /// session or trusted for a user (P4-01).
    Owner,
    /// A server this one is paired with, its devices, or a portable user
    /// granted access (P7, P8). None until they exist.
    Friend,
    /// Anyone else.
    Stranger,
    /// A tier this build does not know: a stranger.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec;

    #[test]
    fn a_query_and_its_answer_go_both_ways() {
        let query = AccessQuery {
            endpoint: "3f9a".into(),
        };
        let back: AccessQuery = codec::decode(&codec::encode(&query).unwrap()).unwrap();
        assert_eq!(back, query);
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
}
