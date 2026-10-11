//! P7-08: security key roots, against vectors computed apart from this
//! code (`vectors/p7_08.py`, Python's `cryptography` and `blake3`).

use ed25519_dalek::{Signer, SigningKey};
use serde_json::Value;
use spoolframe_protocol::portable::{
    AddedRoot, DevicePublicKey, IdentityGenesis, PortableIdentityState, PortableUserId,
    RemovedRoot, RootKey, RootSignature, StateChanges, StateError, UserDeviceCredential,
    root_add_message, root_endorse_message, root_set,
};

/// 2026-10-09T00:00:00Z.
const T: i64 = 1_791_504_000;
const DAY: i64 = 24 * 60 * 60;

fn vectors() -> Value {
    serde_json::from_str(include_str!("vectors/p7_08.json")).unwrap()
}

fn bytes(text: &Value) -> Vec<u8> {
    let text = text.as_str().unwrap();
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn root(v: &Value) -> RootKey {
    RootKey::SecurityKey {
        public_key: bytes(&v["public_key"]).try_into().unwrap(),
        credential_id: bytes(&v["credential_id"]),
    }
}

fn assertion(v: &Value) -> RootSignature {
    RootSignature::Ctap {
        authenticator_data: bytes(&v["authenticator_data"]),
        signature: bytes(&v["signature"]),
    }
}

fn user(v: &Value) -> PortableUserId {
    v.as_str().unwrap().parse().unwrap()
}

fn genesis() -> IdentityGenesis {
    let v = vectors();
    IdentityGenesis {
        handle: bytes(&v["handle"]).try_into().unwrap(),
        roots: vec![root(&v["a"]), root(&v["b"])],
        signatures: v["genesis_signatures"]
            .as_array()
            .unwrap()
            .iter()
            .map(assertion)
            .collect(),
    }
}

/// The credential the vectors lay out, signed by `by` (`a` or `b`).
fn credential(by: &str, signature: &str) -> UserDeviceCredential {
    let v = vectors();
    UserDeviceCredential {
        user: user(&v["user"]),
        device_key: v["device_9"]
            .as_str()
            .unwrap()
            .parse::<DevicePublicKey>()
            .unwrap(),
        serial: 3,
        issued_at: T,
        expires_at: T + 90 * DAY,
        signature: assertion(&v[signature]),
        root: Some(root(&v[by])),
        genesis: None,
    }
}

#[test]
fn a_genesis_of_two_keys_lays_out_verifies_and_derives_its_id() {
    let v = vectors();
    let genesis = genesis();
    assert_eq!(
        IdentityGenesis::message(&genesis.handle, &genesis.roots).unwrap(),
        bytes(&v["genesis_message"])
    );
    assert_eq!(genesis.verify(), Ok(()));
    assert_eq!(genesis.user(), user(&v["user"]));
}

#[test]
fn a_genesis_missing_a_root_s_signature_is_refused() {
    let mut genesis = genesis();
    genesis.signatures.swap(0, 1);
    assert!(genesis.verify().is_err());
    let mut genesis = self::genesis();
    genesis.signatures.pop();
    assert!(genesis.verify().is_err());
}

#[test]
fn a_genesis_of_one_phrase_root_keeps_the_phrase_id() {
    let phrase = SigningKey::from_bytes(&[7; 32]);
    let root = RootKey::Phrase(phrase.verifying_key().to_bytes());
    let handle = [0x44; 32];
    let message = IdentityGenesis::message(&handle, std::slice::from_ref(&root)).unwrap();
    let genesis = IdentityGenesis {
        handle,
        roots: vec![root],
        signatures: vec![phrase.sign(&message).into()],
    };
    assert_eq!(genesis.verify(), Ok(()));
    assert_eq!(
        genesis.user().as_bytes(),
        &phrase.verifying_key().to_bytes()
    );
}

#[test]
fn a_credential_either_key_signed_verifies_and_its_message_is_the_layout() {
    let v = vectors();
    for (by, signature) in [("a", "credential_by_a"), ("b", "credential_by_b")] {
        let credential = credential(by, signature);
        assert_eq!(
            UserDeviceCredential::message(
                &credential.user,
                &credential.device_key,
                3,
                T,
                T + 90 * DAY
            ),
            bytes(&v["credential_message"])
        );
        assert_eq!(credential.verify(T), Ok(()), "{by}");
    }
    let roots = root_set(&user(&v["user"]), Some(&genesis()), &[]);
    assert!(roots.holds(&root(&v["a"])));
    assert!(roots.holds(&root(&v["b"])));
    assert!(!roots.holds(&root(&v["c"])));
}

#[test]
fn an_assertion_without_touch_or_pin_or_under_another_rp_is_refused() {
    for refused in [
        "credential_by_a_up_only",
        "credential_by_a_uv_only",
        "credential_by_a_other_rp",
    ] {
        assert!(credential("a", refused).verify(T).is_err(), "{refused}");
    }
}

#[test]
fn an_assertion_over_other_bytes_or_by_another_key_is_refused() {
    // B's assertion presented as A's.
    assert!(credential("a", "credential_by_b").verify(T).is_err());
    // A's assertion over a credential with another serial.
    let moved = UserDeviceCredential {
        serial: 4,
        ..credential("a", "credential_by_a")
    };
    assert!(moved.verify(T).is_err());
}

#[test]
fn an_oversized_assertion_is_refused() {
    let good = credential("a", "credential_by_a");
    let RootSignature::Ctap {
        authenticator_data,
        signature,
    } = good.signature.clone()
    else {
        unreachable!()
    };
    let mut long_data = authenticator_data.clone();
    long_data.resize(513, 0);
    let mut long_signature = signature.clone();
    long_signature.resize(73, 0);
    for signature in [
        RootSignature::Ctap {
            authenticator_data: long_data,
            signature: signature.clone(),
        },
        RootSignature::Ctap {
            authenticator_data: authenticator_data[..36].to_vec(),
            signature: signature.clone(),
        },
        RootSignature::Ctap {
            authenticator_data,
            signature: long_signature,
        },
    ] {
        let credential = UserDeviceCredential {
            signature,
            ..good.clone()
        };
        assert!(credential.verify(T).is_err());
    }
    // Nor does one read from the wire.
    let json = serde_json::json!({
        "authenticator_data": "00".repeat(513),
        "signature": "00",
    });
    assert!(serde_json::from_value::<RootSignature>(json).is_err());
}

/// A's state at revision 2: C added, B removed an hour before [`T`].
fn state_by_a() -> PortableIdentityState {
    let v = vectors();
    PortableIdentityState {
        user: user(&v["user"]),
        revision: 2,
        revoked: vec![],
        issued_at: T,
        signature: assertion(&v["state_by_a"]),
        signer: Some(root(&v["a"])),
        roots_added: vec![AddedRoot {
            root: root(&v["c"]),
            added_by: root(&v["a"]),
            added_at: T - 7200,
            endorsement: assertion(&v["endorse_c_by_a"]),
            possession: assertion(&v["add_c_possession"]),
        }],
        roots_removed: vec![RemovedRoot {
            root: root(&v["b"]),
            removed_at: T - 3600,
        }],
    }
}

#[test]
fn a_state_that_changes_roots_lays_out_and_verifies() {
    let v = vectors();
    let state = state_by_a();
    assert_eq!(
        root_add_message(&state.user, &root(&v["c"])),
        bytes(&v["add_c_message"])
    );
    assert_eq!(
        root_endorse_message(&state.user, &root(&v["c"]), T - 7200),
        bytes(&v["endorse_c_message"])
    );
    assert_eq!(state.signed_message(), bytes(&v["state_message"]));
    assert_eq!(state.verify(), Ok(()));
    // It round-trips through serde as it is.
    let back: PortableIdentityState =
        serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    assert_eq!(back.verify(), Ok(()));
}

#[test]
fn a_removed_root_s_credentials_no_longer_hold_and_its_backup_s_do() {
    let v = vectors();
    let state = state_by_a();
    let roots = root_set(&state.user, Some(&genesis()), std::slice::from_ref(&state));
    assert!(roots.holds(&root(&v["a"])));
    assert!(roots.holds(&root(&v["c"])));
    assert!(!roots.holds(&root(&v["b"])));
    // B's credential still verifies as a signature: the set refuses it.
    assert_eq!(credential("b", "credential_by_b").verify(T), Ok(()));
    assert!(!roots.holds(&credential("b", "credential_by_b").signer()));
}

#[test]
fn a_root_added_without_its_own_signature_is_refused() {
    let v = vectors();
    let mut state = state_by_a();
    // C's addition, "proved" by A's assertion over the credential.
    state.roots_added[0].possession = assertion(&v["credential_by_a"]);
    assert_eq!(state.verify(), Err(StateError::BadPossession));
}

#[test]
fn a_phrase_identity_adds_a_key_and_keeps_its_id() {
    let v = vectors();
    let phrase = SigningKey::from_bytes(&[7; 32]);
    let user = user(&v["phrase_user"]);
    assert_eq!(user.as_bytes(), &phrase.verifying_key().to_bytes());
    assert_eq!(
        root_add_message(&user, &root(&v["a"])),
        bytes(&v["phrase_add_a_message"])
    );
    let state = PortableIdentityState::signed_by(
        user,
        RootKey::phrase_of(&user),
        5,
        StateChanges {
            roots_added: vec![AddedRoot {
                root: root(&v["a"]),
                added_by: RootKey::phrase_of(&user),
                added_at: T,
                endorsement: phrase
                    .sign(&root_endorse_message(&user, &root(&v["a"]), T))
                    .into(),
                possession: assertion(&v["phrase_add_a_possession"]),
            }],
            ..StateChanges::default()
        },
        T,
        |message| phrase.sign(message).into(),
    )
    .unwrap();
    assert_eq!(state.verify(), Ok(()));
    // Its signer is the phrase root, left unnamed as before P7-08.
    assert_eq!(state.signer, None);
    let roots = root_set(&user, None, std::slice::from_ref(&state));
    assert!(roots.holds(&RootKey::phrase_of(&user)));
    assert!(roots.holds(&root(&v["a"])));
    // A credential the key signs for the phrase identity holds by it.
    let device =
        DevicePublicKey::from_bytes(SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes())
            .unwrap();
    let by_phrase = UserDeviceCredential::new(user, device, 1, T, T + DAY, |m| phrase.sign(m));
    assert_eq!(by_phrase.verify(T), Ok(()));
    assert!(roots.holds(&by_phrase.signer()));
}

/// Phrase roots `seed`, for the root-set tests: 7 is the identity's own.
fn phrase(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn seven() -> PortableUserId {
    PortableUserId::from_bytes(phrase(7).verifying_key().to_bytes()).unwrap()
}

fn phrase_root(seed: u8) -> RootKey {
    RootKey::Phrase(phrase(seed).verifying_key().to_bytes())
}

/// Root `seed` added by root `by` at `at`.
fn added(seed: u8, by: u8, at: i64) -> AddedRoot {
    let user = seven();
    let root = phrase_root(seed);
    AddedRoot {
        root: root.clone(),
        added_by: phrase_root(by),
        added_at: at,
        endorsement: phrase(by)
            .sign(&root_endorse_message(&user, &root, at))
            .into(),
        possession: phrase(seed).sign(&root_add_message(&user, &root)).into(),
    }
}

fn removed(seed: u8, at: i64) -> RemovedRoot {
    RemovedRoot {
        root: phrase_root(seed),
        removed_at: at,
    }
}

/// A state of identity 7 signed by phrase root `by`.
fn state(
    by: u8,
    revision: u64,
    add: Vec<AddedRoot>,
    remove: Vec<RemovedRoot>,
) -> PortableIdentityState {
    let state = PortableIdentityState::signed_by(
        seven(),
        phrase_root(by),
        revision,
        StateChanges {
            roots_added: add,
            roots_removed: remove,
            ..StateChanges::default()
        },
        T,
        |m| phrase(by).sign(m).into(),
    )
    .unwrap();
    assert_eq!(state.verify(), Ok(()));
    state
}

#[test]
fn rival_states_that_remove_each_other_leave_both_removed() {
    // 7 adds 8; then each removes the other.
    let add = state(7, 1, vec![added(8, 7, T)], vec![]);
    let by_seven = state(7, 2, vec![added(8, 7, T)], vec![removed(8, T + 10)]);
    let by_eight = state(8, 2, vec![added(8, 7, T)], vec![removed(7, T + 10)]);
    // In either order: a set is worked out from all of them at once.
    for states in [
        vec![add.clone(), by_seven.clone(), by_eight.clone()],
        vec![by_eight, add, by_seven],
    ] {
        let roots = root_set(&seven(), None, &states);
        assert!(!roots.holds(&phrase_root(7)));
        assert!(!roots.holds(&phrase_root(8)));
    }
}

#[test]
fn a_root_added_by_a_key_later_rotated_out_stays() {
    // 7 adds 8; 8 adds 9; 9 removes 8 and 7: only 9 is left, and its
    // own state carries the whole chain.
    let rotated = state(
        9,
        3,
        vec![added(8, 7, T), added(9, 8, T + 10)],
        vec![removed(8, T + 20), removed(7, T + 20)],
    );
    let roots = root_set(&seven(), None, std::slice::from_ref(&rotated));
    assert!(roots.holds(&phrase_root(9)));
    assert!(!roots.holds(&phrase_root(8)));
    assert!(!roots.holds(&phrase_root(7)));
}

#[test]
fn a_removed_root_cannot_add_another_after_its_removal() {
    // 7 adds 8, then removes it; 8 (stolen) adds 9 of the thief's later.
    let add = state(7, 1, vec![added(8, 7, T)], vec![]);
    let remove = state(7, 2, vec![added(8, 7, T)], vec![removed(8, T + 10)]);
    let thief = state(8, 3, vec![added(8, 7, T), added(9, 8, T + 20)], vec![]);
    let roots = root_set(&seven(), None, &[add, remove, thief]);
    assert!(roots.holds(&phrase_root(7)));
    assert!(!roots.holds(&phrase_root(8)));
    assert!(!roots.holds(&phrase_root(9)));
}

#[test]
fn an_addition_its_adder_did_not_endorse_is_refused() {
    // 9 claims 8 added it, but 9 signed the endorsement itself.
    let user = seven();
    let mut forged = added(9, 8, T);
    forged.endorsement = phrase(9)
        .sign(&root_endorse_message(&user, &phrase_root(9), T))
        .into();
    let state = PortableIdentityState::signed_by(
        user,
        phrase_root(7),
        1,
        StateChanges {
            roots_added: vec![forged],
            ..StateChanges::default()
        },
        T,
        |m| phrase(7).sign(m).into(),
    )
    .unwrap();
    assert_eq!(state.verify(), Err(StateError::BadPossession));
}
