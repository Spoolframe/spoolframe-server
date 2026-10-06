//! A server's id: its name for app passkeys, apart from its key.
//!
//! The endpoint key is what a client pins and proves on every connect, and
//! it may be replaced (a lost state directory, a rotated key). A passkey
//! is made for a relying party and synced with it to the user's other
//! devices, so the server needs a name that outlives its key: the server
//! id. It is made once, from 80 random bits, and kept in the state
//! directory beside the key.
//!
//! It is written as lowercase RFC 4648 base32: 16 characters of `a`-`z`
//! and `2`-`7`, with no padding (80 bits are 16 characters exactly), a DNS
//! label as it is. It is short because a password manager shows it in
//! the passkey's domain, and it says nothing about the server: it goes
//! wherever its passkeys go.

use std::fmt;
use std::str::FromStr;

pub use spoolframe_types::APP_RP_DOMAIN;

/// RFC 4648's base32 alphabet, lowercase.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Random bytes in a server id.
pub const SERVER_ID_BYTES: usize = 10;

/// Characters in a server id: 80 bits in 5-bit characters.
pub const SERVER_ID_LEN: usize = 16;

/// A server's id, as [`ServerId::parse`] checked it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerId(String);

/// Why a string is not a server id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a server id: 16 characters of a-z and 2-7")]
pub struct InvalidServerId(pub String);

impl ServerId {
    /// The id written for `bytes`: the server makes them at random.
    pub fn from_bytes(bytes: [u8; SERVER_ID_BYTES]) -> Self {
        let mut id = String::with_capacity(SERVER_ID_LEN);
        // 5 bytes are 8 characters, with no bits left over.
        for group in bytes.as_chunks::<5>().0 {
            let bits = group
                .iter()
                .fold(0u64, |bits, &byte| (bits << 8) | u64::from(byte));
            for at in (0..8).rev() {
                id.push(char::from(ALPHABET[((bits >> (at * 5)) & 31) as usize]));
            }
        }
        Self(id)
    }

    /// `id` if it is a server id as [`ServerId::from_bytes`] writes one:
    /// 16 characters of the alphabet. Anything else, uppercase too, is
    /// refused: a server id names a relying party, and one id has one
    /// spelling.
    pub fn parse(id: &str) -> Result<Self, InvalidServerId> {
        if id.len() == SERVER_ID_LEN && id.bytes().all(|c| ALPHABET.contains(&c)) {
            Ok(Self(id.to_owned()))
        } else {
            Err(InvalidServerId(id.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The relying party of this server's app passkeys:
    /// `<id>.`[`APP_RP_DOMAIN`]. A security key keeps passkeys apart by
    /// relying party, so one with passkeys for two servers offers only
    /// this server's.
    pub fn rp_id(&self) -> String {
        format!("{}.{APP_RP_DOMAIN}", self.0)
    }

    /// The origin the platform writes into `clientDataJSON` for
    /// [`ServerId::rp_id`]: `https://<id>.spoolframe.app`. WebAuthn wants
    /// the origin's host to be the relying party or under it, so it is
    /// never `https://spoolframe.app`.
    pub fn origin(&self) -> String {
        format!("https://{}", self.rp_id())
    }
}

impl fmt::Display for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ServerId {
    type Err = InvalidServerId;

    fn from_str(id: &str) -> Result<Self, Self::Err> {
        Self::parse(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 base32, lowercase, of 10 bytes: the known answer, from
    /// Python's `base64.b32encode`.
    #[test]
    fn the_id_is_lowercase_base32_of_the_bytes() {
        assert_eq!(
            ServerId::from_bytes(*b"foobarfoob").as_str(),
            "mzxw6ytbojtg633c"
        );
        assert_eq!(ServerId::from_bytes([0; 10]).as_str(), "aaaaaaaaaaaaaaaa");
        assert_eq!(
            ServerId::from_bytes([0xff; 10]).as_str(),
            "7777777777777777"
        );
    }

    /// A DNS label: 1 to 63 letters, digits and hyphens, not starting or
    /// ending with a hyphen. A relying party's host is made of them.
    fn is_dns_label(label: &str) -> bool {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    }

    #[test]
    fn every_id_is_a_dns_label_of_16_characters() {
        for seed in 0..=255u8 {
            let bytes = std::array::from_fn(|i| seed.wrapping_mul(31) ^ (i as u8).wrapping_mul(97));
            let id = ServerId::from_bytes(bytes);
            assert_eq!(id.as_str().len(), SERVER_ID_LEN, "{id}");
            assert!(is_dns_label(id.as_str()), "{id}");
            assert_eq!(ServerId::parse(id.as_str()), Ok(id));
        }
    }

    #[test]
    fn the_relying_party_and_origin_are_under_spoolframe_app() {
        let id = ServerId::parse("mzxw6ytbojtg633c").unwrap();
        assert_eq!(id.rp_id(), "mzxw6ytbojtg633c.spoolframe.app");
        assert_eq!(id.origin(), "https://mzxw6ytbojtg633c.spoolframe.app");
    }

    #[test]
    fn what_is_not_an_id_as_written_is_refused() {
        for bad in [
            "",
            "mzxw6ytboj",
            "mzxw6ytbojtg633ca",
            "MZXW6YTBOJTG633C",
            "mzxw6ytboj1g633c",
            "mzxw6ytboj.g633c",
            "mzxw6ytboj-g633c",
        ] {
            assert_eq!(
                ServerId::parse(bad),
                Err(InvalidServerId(bad.into())),
                "{bad}"
            );
        }
    }
}
