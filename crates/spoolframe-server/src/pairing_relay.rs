//! Pairing through this server (P7-08): a device with no one to scan its
//! QR code (a TV, when the root is on a desktop with no camera) holds its
//! pairing request here under a short code it shows; the root terminal
//! types the code, fetches the request, and posts its credential back for
//! the device to collect.
//!
//! The server only carries signed data. It checks that an answer is for
//! the key the request named, and nothing else: the device checks the
//! credential is for its own key and shows whose identity it is before it
//! keeps it, so a server that swapped the key, or a stranger who guessed
//! the code, is caught there. Codes are guessed only at the strangers'
//! rate (`crate::client`), and each is tried at most
//! [`MAX_FETCHES`] times.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use spoolframe_protocol::pairing::{
    MAX_NAME, MAX_PAIRED_SERVERS, PairingAnswer, PairingOffer, RELAY_TTL_SECS, RelayCode,
};

use crate::clock::Clock;

/// How long a request is held, from its offer.
pub const RELAY_TTL: Duration = Duration::from_secs(RELAY_TTL_SECS);

/// The most requests held at once. Each offer draws on the strangers'
/// budget, so this is a backstop, never reached by people.
const MAX_HELD: usize = 1_000;

/// The most times one request is fetched before it is dropped: a root
/// terminal fetches it once or twice; more is someone guessing.
pub const MAX_FETCHES: u8 = 5;

/// Why a relay request is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RelayError {
    /// No request under that code (or it is over, used up, answered, or
    /// another device's).
    #[error("no pairing request under that code")]
    NotFound,
    /// Another device holds the code.
    #[error("another device holds that code")]
    Taken,
    /// An answer for another key than the request's, or too many servers.
    #[error("the answer is not for the request's key")]
    WrongAnswer,
    /// The server holds as many as it takes.
    #[error("too many pairing requests held")]
    Full,
}

#[derive(Debug)]
struct Held {
    /// The endpoint that offered it: the only one that may collect.
    endpoint: String,
    offer: PairingOffer,
    answer: Option<PairingAnswer>,
    offered: Instant,
    fetches: u8,
}

/// The pairing requests held here, by code.
#[derive(Debug)]
pub struct PairingRelay {
    clock: Clock,
    held: Mutex<HashMap<RelayCode, Held>>,
}

impl PairingRelay {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            held: Mutex::default(),
        }
    }

    /// Hold `offer` from `endpoint`, replacing that endpoint's earlier
    /// one; a code another endpoint holds is [`RelayError::Taken`].
    pub fn offer(&self, endpoint: &str, mut offer: PairingOffer) -> Result<(), RelayError> {
        offer.name = offer.name.chars().take(MAX_NAME).collect();
        let now = self.clock.now();
        let mut held = self.held.lock();
        held.retain(|_, h| now.duration_since(h.offered) < RELAY_TTL);
        if held
            .get(&offer.code)
            .is_some_and(|h| h.endpoint != endpoint)
        {
            return Err(RelayError::Taken);
        }
        held.retain(|_, h| h.endpoint != endpoint);
        if held.len() >= MAX_HELD {
            return Err(RelayError::Full);
        }
        held.insert(
            offer.code.clone(),
            Held {
                endpoint: endpoint.to_owned(),
                offer,
                answer: None,
                offered: now,
                fetches: 0,
            },
        );
        Ok(())
    }

    /// The request under `code`, counted: after [`MAX_FETCHES`] it is
    /// dropped. One already answered is not handed out again.
    pub fn fetch(&self, code: &RelayCode) -> Result<PairingOffer, RelayError> {
        let now = self.clock.now();
        let mut held = self.held.lock();
        let entry = held.get_mut(code).ok_or(RelayError::NotFound)?;
        if now.duration_since(entry.offered) >= RELAY_TTL || entry.answer.is_some() {
            return Err(RelayError::NotFound);
        }
        entry.fetches += 1;
        let offer = entry.offer.clone();
        if entry.fetches >= MAX_FETCHES {
            held.remove(code);
        }
        Ok(offer)
    }

    /// Take `answer` for the request under `code`: once, and only for the
    /// key it named.
    pub fn answer(&self, code: &RelayCode, answer: PairingAnswer) -> Result<(), RelayError> {
        if answer.servers.len() > MAX_PAIRED_SERVERS {
            return Err(RelayError::WrongAnswer);
        }
        let now = self.clock.now();
        let mut held = self.held.lock();
        let entry = held.get_mut(code).ok_or(RelayError::NotFound)?;
        if now.duration_since(entry.offered) >= RELAY_TTL || entry.answer.is_some() {
            return Err(RelayError::NotFound);
        }
        if answer.credential.device_key != entry.offer.device_key {
            return Err(RelayError::WrongAnswer);
        }
        entry.answer = Some(answer);
        Ok(())
    }

    /// The answer to `endpoint`'s request under `code`: taken, ending the
    /// request, or `None` while there is none yet.
    pub fn collect(
        &self,
        endpoint: &str,
        code: &RelayCode,
    ) -> Result<Option<PairingAnswer>, RelayError> {
        let now = self.clock.now();
        let mut held = self.held.lock();
        let entry = held.get(code).ok_or(RelayError::NotFound)?;
        if entry.endpoint != endpoint || now.duration_since(entry.offered) >= RELAY_TTL {
            return Err(RelayError::NotFound);
        }
        if entry.answer.is_none() {
            return Ok(None);
        }
        Ok(held.remove(code).and_then(|h| h.answer))
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use spoolframe_protocol::portable::{
        DevicePublicKey, MAX_CREDENTIAL_VALIDITY, PortableUserId, UserDeviceCredential,
    };

    use super::*;

    const T: i64 = 1_791_504_000;

    fn code(text: &str) -> RelayCode {
        RelayCode::parse(text).unwrap()
    }

    fn key(seed: u8) -> DevicePublicKey {
        DevicePublicKey::from_bytes(
            SigningKey::from_bytes(&[seed; 32])
                .verifying_key()
                .to_bytes(),
        )
        .unwrap()
    }

    fn offer(text: &str, seed: u8) -> PairingOffer {
        PairingOffer {
            code: code(text),
            device_key: key(seed),
            name: "Living room".into(),
        }
    }

    fn answer(seed: u8) -> PairingAnswer {
        let root = SigningKey::from_bytes(&[7; 32]);
        let user = PortableUserId::from_bytes(root.verifying_key().to_bytes()).unwrap();
        PairingAnswer {
            credential: Box::new(UserDeviceCredential::new(
                user,
                key(seed),
                1,
                T,
                T + MAX_CREDENTIAL_VALIDITY,
                |m| root.sign(m),
            )),
            state: None,
            servers: vec![],
        }
    }

    #[test]
    fn a_request_is_fetched_answered_and_collected_by_its_device_once() {
        let relay = PairingRelay::new(Clock::default());
        relay.offer("tv", offer("ABCDE-FGHJK", 9)).unwrap();
        // Typed in lowercase, with I for 1: the same code.
        assert_eq!(relay.fetch(&code("abcdefghjk")).unwrap().device_key, key(9));
        assert_eq!(relay.collect("tv", &code("ABCDEFGHJK")), Ok(None));
        relay.answer(&code("ABCDEFGHJK"), answer(9)).unwrap();
        // Not by another device, nor answered twice.
        assert_eq!(
            relay.collect("laptop", &code("ABCDEFGHJK")),
            Err(RelayError::NotFound)
        );
        assert_eq!(
            relay.answer(&code("ABCDEFGHJK"), answer(9)),
            Err(RelayError::NotFound)
        );
        let collected = relay.collect("tv", &code("ABCDEFGHJK")).unwrap().unwrap();
        assert_eq!(collected.credential.device_key, key(9));
        assert_eq!(
            relay.collect("tv", &code("ABCDEFGHJK")),
            Err(RelayError::NotFound)
        );
    }

    #[test]
    fn an_answer_for_another_key_is_refused() {
        let relay = PairingRelay::new(Clock::default());
        relay.offer("tv", offer("ABCDEFGHJK", 9)).unwrap();
        assert_eq!(
            relay.answer(&code("ABCDEFGHJK"), answer(11)),
            Err(RelayError::WrongAnswer)
        );
        relay.answer(&code("ABCDEFGHJK"), answer(9)).unwrap();
    }

    #[test]
    fn a_code_another_device_holds_is_taken_and_one_device_holds_one() {
        let relay = PairingRelay::new(Clock::default());
        relay.offer("tv", offer("ABCDEFGHJK", 9)).unwrap();
        assert_eq!(
            relay.offer("laptop", offer("ABCDEFGHJK", 11)),
            Err(RelayError::Taken)
        );
        // The same device offering again replaces its own.
        relay.offer("tv", offer("MNPQRSTVWX", 9)).unwrap();
        assert_eq!(relay.fetch(&code("ABCDEFGHJK")), Err(RelayError::NotFound));
        relay.fetch(&code("MNPQRSTVWX")).unwrap();
    }

    #[test]
    fn a_request_is_dropped_after_its_fetches_or_its_time() {
        let clock = Clock::default();
        let relay = PairingRelay::new(clock.clone());
        relay.offer("tv", offer("ABCDEFGHJK", 9)).unwrap();
        for _ in 0..MAX_FETCHES {
            relay.fetch(&code("ABCDEFGHJK")).unwrap();
        }
        assert_eq!(relay.fetch(&code("ABCDEFGHJK")), Err(RelayError::NotFound));
        relay.offer("tv", offer("ABCDEFGHJK", 9)).unwrap();
        clock.advance(RELAY_TTL);
        assert_eq!(relay.fetch(&code("ABCDEFGHJK")), Err(RelayError::NotFound));
        assert_eq!(
            relay.collect("tv", &code("ABCDEFGHJK")),
            Err(RelayError::NotFound)
        );
    }
}
