//! A server key's fingerprint (P1-21, ARCHITECTURE §19): a short code a
//! person can compare between the server's terminal and a client's screen
//! before the client pins the key.
//!
//! SHA-256 over a fixed label and the 32-byte `EndpointId`, the first 80
//! bits, in Crockford base32, as four groups of four: `K7QX-3MPA-9TRD-WF2H`.
//! 80 bits puts making a key with a given fingerprint out of reach; a
//! six-digit code could be matched by generating keys for a few seconds.
//! Crockford's alphabet has no I, L, O or U, so the code reads aloud
//! without confusion. The server and every client compute it here, so they
//! always agree.

use sha2::{Digest, Sha256};

/// Changing it changes every fingerprint; a new scheme gets a new label.
const LABEL: &[u8] = b"spoolframe fingerprint v1";

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The fingerprint of the `EndpointId` whose 32 bytes are `key`.
pub fn fingerprint(key: &[u8; 32]) -> String {
    let digest = Sha256::new()
        .chain_update(LABEL)
        .chain_update(key)
        .finalize();
    let mut bits = [0u8; 16];
    bits[6..].copy_from_slice(&digest[..10]);
    let n = u128::from_be_bytes(bits);
    let mut out = String::with_capacity(19);
    for i in 0..16 {
        if i > 0 && i % 4 == 0 {
            out.push('-');
        }
        let digit = (n >> (5 * (15 - i))) & 31;
        out.push(char::from(CROCKFORD[digit as usize]));
    }
    out
}

/// The fingerprint of an `EndpointId` written as iroh prints it (64 hex
/// digits); `None` if it is not one.
pub fn of_hex(endpoint_id: &str) -> Option<String> {
    if endpoint_id.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(endpoint_id.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(fingerprint(&key))
}

/// A fingerprint as a person typed it, read back from another screen or
/// aloud: in the form [`fingerprint`] writes it, or `None` if it is not
/// one. Case, spaces and dashes do not matter, and the letters Crockford
/// reads as digits are read so (`O` as `0`, `I` and `L` as `1`); anything
/// else, and any length but sixteen characters, is not a fingerprint.
pub fn parse(typed: &str) -> Option<String> {
    let mut digits = String::with_capacity(16);
    for c in typed.chars() {
        let c = match c.to_ascii_uppercase() {
            '-' | ' ' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c if c.is_ascii() && CROCKFORD.contains(&(c as u8)) => c,
            _ => return None,
        };
        digits.push(c);
    }
    if digits.len() != 16 {
        return None;
    }
    let mut out = String::with_capacity(19);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            out.push('-');
        }
        out.push(c);
    }
    Some(out)
}

/// Keys and their fingerprints, computed apart from this code (Python's
/// hashlib), so the format cannot drift. The server's and the SDK's tests
/// check them too.
pub const TEST_VECTORS: [(&str, &str); 3] = [
    (
        "0000000000000000000000000000000000000000000000000000000000000000",
        "PGJA-BHZ1-F68M-DFSE",
    ),
    (
        "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20",
        "QAJ7-XQG9-2MDW-AN9H",
    ),
    (
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "NWXF-HYJ0-Y9VH-1R2M",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_vectors_hold() {
        for (key, expected) in TEST_VECTORS {
            assert_eq!(of_hex(key).as_deref(), Some(expected), "{key}");
        }
    }

    #[test]
    fn only_crockford_letters_are_used() {
        let code = of_hex(TEST_VECTORS[1].0).unwrap();
        assert_eq!(code.len(), 19);
        assert!(!code.contains(['I', 'L', 'O', 'U']));
    }

    #[test]
    fn a_string_that_is_not_a_key_has_no_fingerprint() {
        assert_eq!(of_hex("abc"), None);
        assert_eq!(of_hex(&"zz".repeat(32)), None);
    }

    #[test]
    fn a_typed_fingerprint_is_read_in_the_written_form() {
        let written = TEST_VECTORS[1].1;
        for typed in [
            written,
            "qaj7-xqg9-2mdw-an9h",
            "QAJ7 XQG9 2MDW AN9H",
            "QAJ7XQG92MDWAN9H",
        ] {
            assert_eq!(parse(typed).as_deref(), Some(written), "{typed}");
        }
        assert_eq!(
            parse("PGJA-BHZI-F68M-DFSE").as_deref(),
            Some(TEST_VECTORS[0].1)
        );
        assert_eq!(
            parse("NWXF-HYJO-Y9VH-LR2M").as_deref(),
            Some(TEST_VECTORS[2].1)
        );
    }

    #[test]
    fn what_is_not_a_fingerprint_is_not_read_as_one() {
        for typed in [
            "",
            "QAJ7-XQG9-2MDW",
            "QAJ7-XQG9-2MDW-AN9H-0",
            "QAJ7-XQG9-2MDW-AN9U",
            "QAJ7_XQG9_2MDW_AN9H",
            "QAJ7-XQG9-2MDW-AN9É",
        ] {
            assert_eq!(parse(typed), None, "{typed}");
        }
    }
}
