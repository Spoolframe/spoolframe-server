//! Signing a TV in with a phone: the phone hands the TV the servers it
//! signs in to and a sign-in it made for the TV on each, over a direct
//! Iroh connection on [`SIGNIN_ALPN`].
//!
//! The TV shows the pairing link of [`crate::pairing`] as a QR code: the
//! same link, the same one-time code. The phone dials the TV's key on
//! [`SIGNIN_ALPN`] to sign it in to accounts, or on
//! [`crate::pairing::PAIR_ALPN`] to put a guest's own identity on it;
//! the code is taken by whichever comes first.
//!
//! On one bidirectional stream, each frame a [`crate::codec`] frame of
//! at most [`MAX_FRAME`] bytes:
//!
//! 1. phone → TV: [`PhoneFrame::Hello`], the code;
//! 2. TV → phone: [`TvFrame::Device`], what the TV is, or
//!    [`TvFrame::Refused`];
//! 3. phone → TV: [`PhoneFrame::Offers`], one [`Offer`] for each server
//!    the person picked, at most [`MAX_OFFERS`], or
//!    [`PhoneFrame::Declined`];
//! 4. TV → phone: [`TvFrame::Done`], how each offer went, in the order
//!    of the offers.
//!
//! An offer's `request` is a device sign-in the phone's session made on
//! that server for the TV's key (`OfferDeviceSignIn`). The TV redeems it
//! with `AwaitDeviceSignIn` as if it had begun it itself; the server
//! answers only the endpoint it was made for.
//!
//! Each enum has an `Unknown` variant, so a newer peer may add frames and
//! outcomes: one read as `Unknown` ends the exchange, as one out of turn
//! does.

use serde::{Deserialize, Serialize};
use spoolframe_types::Platform;

use crate::pairing::{MAX_PAIRED_SERVERS, PairedServer, PairingCode};

/// The ALPN of signing in with a phone, version 1. A TV accepts it only
/// while it shows its code, whether or not it allows guests.
pub const SIGNIN_ALPN: &[u8] = b"spoolframe/signin/1";

/// The largest frame either side reads: [`MAX_OFFERS`] servers at their
/// limits fit many times over.
pub const MAX_FRAME: u32 = crate::pairing::MAX_FRAME;

/// The most offers a [`PhoneFrame::Offers`] carries; a TV reads no more.
pub const MAX_OFFERS: usize = MAX_PAIRED_SERVERS;

/// The most characters of a reason a [`OfferOutcome::Failed`] carries.
pub const MAX_REASON: usize = 200;

/// What the phone sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PhoneFrame {
    /// The code from the QR code. First, and only once.
    Hello { code: PairingCode },
    /// The sign-ins the phone made for the TV, one per server the person
    /// picked.
    Offers { offers: Vec<Offer> },
    /// The person on the phone said no.
    Declined,
    /// A frame this build does not know.
    #[serde(other)]
    Unknown,
}

/// A server the phone signs the TV in to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    /// The server as the phone has it saved: the TV pins it, not
    /// verified, as for a pairing's.
    pub server: PairedServer,
    /// The device sign-in made for the TV there (`DeviceSignInOffered`):
    /// the TV's `AwaitDeviceSignIn` redeems it.
    pub request: String,
    /// Who the TV signs in as, for its screen: the profile's display name.
    pub user_name: String,
}

/// What the TV sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TvFrame {
    /// What the TV is: "Sign in Living Room TV?". The phone makes its
    /// offers for this.
    Device {
        name: String,
        platform: Platform,
        client_name: String,
    },
    /// How each offer went, in the order of the offers.
    Done { results: Vec<OfferResult> },
    /// The exchange is over, refused for `reason`.
    Refused { reason: Refusal },
    /// A frame this build does not know.
    #[serde(other)]
    Unknown,
}

/// How one offer went on the TV.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferResult {
    /// The offer's server: its endpoint id, as the offer named it.
    pub server: String,
    pub outcome: OfferOutcome,
}

/// Whether the TV is signed in to an offer's server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OfferOutcome {
    /// The TV holds a session there now.
    SignedIn,
    /// It is not: why, for the phone to show, cut to [`MAX_REASON`]
    /// characters.
    Failed { reason: String },
    /// The person at the TV turned the sign-in down: nothing was
    /// redeemed, and the grant runs out on its server unused.
    Declined,
    /// An outcome this build does not know.
    #[serde(other)]
    Unknown,
}

impl OfferOutcome {
    /// A failure for `reason`, cut to [`MAX_REASON`] characters.
    pub fn failed(reason: &str) -> Self {
        Self::Failed {
            reason: reason.chars().take(MAX_REASON).collect(),
        }
    }
}

/// Why a TV refused a phone sign-in. The TV's code ends with each: it
/// shows a new one to try again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// The code is not the one shown, or was used.
    #[error("the sign-in code does not match")]
    WrongCode,
    /// The code was shown more than [`crate::pairing::CODE_TTL_SECS`] ago.
    #[error("the sign-in code has expired")]
    Expired,
    /// Something else went wrong on the TV.
    #[error("the TV could not finish signing in")]
    Failed,
    /// A reason this build does not know.
    #[serde(other)]
    #[error("the TV refused the sign-in")]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};

    fn home() -> PairedServer {
        PairedServer {
            endpoint_id: "ab".repeat(32),
            server_id: "q7k3mpa9trdwf2h".into(),
            name: "Home Media".into(),
            addrs: vec![
                "192.168.1.10:7420".parse().unwrap(),
                "[2001:db8::1]:7420".parse().unwrap(),
            ],
            relays: vec!["https://relay.example.org/".into()],
        }
    }

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(t: T) {
        assert_eq!(decode::<T>(&encode(&t).unwrap()).unwrap(), t);
    }

    /// The names on the wire are promised to the apps.
    #[test]
    fn the_frames_wire_names_are_fixed() {
        let offers = PhoneFrame::Offers {
            offers: vec![Offer {
                server: home(),
                request: "r1".into(),
                user_name: "Steve".into(),
            }],
        };
        let json = serde_json::to_value(&offers).unwrap();
        assert_eq!(json["type"], "offers");
        assert_eq!(json["offers"][0]["request"], "r1");
        assert_eq!(json["offers"][0]["user_name"], "Steve");
        assert_eq!(
            json["offers"][0]["server"]["addrs"][1],
            "[2001:db8::1]:7420"
        );
        round_trip(offers);
        round_trip(PhoneFrame::Hello {
            code: PairingCode::from_bytes([7; 32]),
        });
        round_trip(PhoneFrame::Declined);

        let device = TvFrame::Device {
            name: "Living Room TV".into(),
            platform: Platform::Tvos,
            client_name: "spoolframe-tvos".into(),
        };
        assert_eq!(
            serde_json::to_value(&device).unwrap(),
            serde_json::json!({
                "type": "device", "name": "Living Room TV", "platform": "tvos",
                "client_name": "spoolframe-tvos",
            })
        );
        round_trip(device);
        let done = TvFrame::Done {
            results: vec![
                OfferResult {
                    server: "ab".repeat(32),
                    outcome: OfferOutcome::SignedIn,
                },
                OfferResult {
                    server: "cd".repeat(32),
                    outcome: OfferOutcome::failed("unreachable"),
                },
            ],
        };
        let json = serde_json::to_value(&done).unwrap();
        assert_eq!(
            json["results"][0]["outcome"],
            serde_json::json!({"type": "signed_in"})
        );
        assert_eq!(
            json["results"][1]["outcome"],
            serde_json::json!({"type": "failed", "reason": "unreachable"})
        );
        round_trip(done);
        let declined = OfferOutcome::Declined;
        assert_eq!(
            serde_json::to_value(&declined).unwrap(),
            serde_json::json!({"type": "declined"})
        );
        round_trip(declined);
        let refused = TvFrame::Refused {
            reason: Refusal::WrongCode,
        };
        assert_eq!(
            serde_json::to_string(&refused).unwrap(),
            r#"{"type":"refused","reason":"wrong_code"}"#
        );
        round_trip(refused);
    }

    /// A newer peer's frame, outcome or reason reads as `Unknown`, through
    /// CBOR as through JSON, rather than failing the read.
    #[test]
    fn what_a_newer_peer_adds_reads_as_unknown() {
        let frame = serde_json::json!({"type": "teleport", "to": "mars"});
        assert_eq!(
            serde_json::from_value::<PhoneFrame>(frame.clone()).unwrap(),
            PhoneFrame::Unknown
        );
        assert_eq!(
            decode::<TvFrame>(&encode(&frame).unwrap()).unwrap(),
            TvFrame::Unknown
        );
        let result = serde_json::json!({
            "server": "ab".repeat(32),
            "outcome": {"type": "postponed", "until": 5},
        });
        assert_eq!(
            decode::<OfferResult>(&encode(&result).unwrap())
                .unwrap()
                .outcome,
            OfferOutcome::Unknown
        );
        let refused = serde_json::json!({"type": "refused", "reason": "too_tired"});
        assert_eq!(
            decode::<TvFrame>(&encode(&refused).unwrap()).unwrap(),
            TvFrame::Refused {
                reason: Refusal::Unknown
            }
        );
    }

    #[test]
    fn a_failure_s_reason_is_cut() {
        let OfferOutcome::Failed { reason } = OfferOutcome::failed(&"é".repeat(500)) else {
            panic!("a failure");
        };
        assert_eq!(reason.chars().count(), MAX_REASON);
    }

    /// Hello carries the code as pairing's does, and Debug keeps it out.
    #[test]
    fn the_hello_s_code_is_kept_out_of_debug() {
        let hello = PhoneFrame::Hello {
            code: PairingCode::from_bytes([7; 32]),
        };
        let shown = format!("{hello:?}");
        assert!(!shown.contains(&PairingCode::from_bytes([7; 32]).to_text()));
        assert!(!shown.contains("7, 7"), "{shown}");
    }
}
