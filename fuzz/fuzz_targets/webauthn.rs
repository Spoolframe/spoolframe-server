//! A security key's WebAuthn registration and assertion (P7-08): what an
//! Apple device's system API hands the SDK and a server, read as a root
//! is. The input is split at a `u16 LE` length into two parts. As a
//! registration they are its `attestationObject` and `clientDataJSON`; as
//! an assertion, its `authenticatorData` and `clientDataJSON` (the
//! signature is the last 72 bytes of the first part). Nothing may panic,
//! and a registration that reads gives a valid root.
#![no_main]

use libfuzzer_sys::fuzz_target;
use spoolframe_protocol::portable::{RootKey, RootRegistration, RootSignature};

/// A fixed root to check assertions against: any P-256 point does.
const POINT: [u8; 65] = [
    0x04, 0x6b, 0x17, 0xd1, 0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5, 0x63, 0xa4, 0x40,
    0xf2, 0x77, 0x03, 0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4, 0xa1, 0x39, 0x45, 0xd8, 0x98, 0xc2,
    0x96, 0x4f, 0xe3, 0x42, 0xe2, 0xfe, 0x1a, 0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a, 0x7c, 0x0f, 0x9e,
    0x16, 0x2b, 0xce, 0x33, 0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6, 0x40, 0x68, 0x37, 0xbf, 0x51,
    0xf5,
];

fuzz_target!(|data: &[u8]| {
    let Some((len, rest)) = data.split_first_chunk::<2>() else {
        return;
    };
    let split = usize::from(u16::from_le_bytes(*len)).min(rest.len());
    let (first, second) = rest.split_at(split);

    let registration = RootRegistration::WebAuthn {
        attestation_object: first.to_vec(),
        client_data_json: second.to_vec(),
    };
    if let Ok(root) = registration.root(&[0x42; 32]) {
        assert!(root.is_valid());
    }

    let (authenticator_data, signature) = first.split_at(first.len().saturating_sub(72));
    let root = RootKey::SecurityKey {
        public_key: POINT,
        credential_id: vec![1; 32],
    };
    let signature = RootSignature::WebAuthn {
        authenticator_data: authenticator_data.to_vec(),
        client_data_json: second.to_vec(),
        signature: signature.to_vec(),
    };
    let _ = root.verify(b"spoolframe fuzz message", &signature);
});
