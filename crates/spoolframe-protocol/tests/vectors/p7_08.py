#!/usr/bin/env python3
"""P7-08's test vectors, computed apart from the Rust code.

Lays out what security key roots sign by hand (`struct`), makes the CTAP2
assertions a FIDO2 key would (`authenticatorData ‖ SHA-256(message)`,
ECDSA P-256 with SHA-256, DER), and derives a key identity's id with
BLAKE3. ECDSA signatures differ each run, so the output is a fixture:
regenerate it only on purpose.

    nix-shell -p 'python3.withPackages(p: [p.cryptography p.blake3])' \
      --run 'python3 p7_08.py > p7_08.json'
"""

import hashlib
import json
import struct

import blake3
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, ed25519

RP = b"portable.spoolframe.app"
GENESIS = b"spoolframe identity genesis v1\0"
ROOT_ADD = b"spoolframe root add v1\0"
ROOT_ENDORSE = b"spoolframe root endorse v1\0"
CREDENTIAL = b"spoolframe device credential v1\0"
STATE_V3 = b"spoolframe identity state v3\0"
USER_ID_CONTEXT = "spoolframe portable user id v1"

# 2026-10-09T00:00:00Z, as P7-02's vectors.
T = 1_791_504_000
DAY = 24 * 60 * 60


def p256(scalar):
    return ec.derive_private_key(scalar, ec.SECP256R1())


def sec1(key):
    return key.public_key().public_bytes(
        serialization.Encoding.X962, serialization.PublicFormat.UncompressedPoint
    )


def ed(seed):
    return ed25519.Ed25519PrivateKey.from_private_bytes(bytes([seed]) * 32)


def ed_public(seed):
    return ed(seed).public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw
    )


def key_root(key, credential_id):
    return b"\x02" + sec1(key) + bytes([len(credential_id)]) + credential_id


def phrase_root(seed):
    return b"\x01" + ed_public(seed)


def auth_data(flags=0x05, counter=0):
    return hashlib.sha256(RP).digest() + bytes([flags]) + struct.pack(">I", counter)


def assert_(key, message, flags=0x05):
    data = auth_data(flags)
    signed = data + hashlib.sha256(message).digest()
    return {
        "authenticator_data": data.hex(),
        "signature": key.sign(signed, ec.ECDSA(hashes.SHA256())).hex(),
    }


A = p256(0xA11CE)
B = p256(0xB0B)
C = p256(0xC0FFEE)
A_ID = b"\xaa" * 16
B_ID = b"\xbb" * 16
C_ID = b"\xcc" * 16
HANDLE = b"\x33" * 32

roots = [key_root(A, A_ID), key_root(B, B_ID)]
genesis_message = GENESIS + HANDLE + bytes([len(roots)]) + b"".join(roots)
user = blake3.blake3(genesis_message, derive_key_context=USER_ID_CONTEXT).digest()

device_9 = ed_public(9)
credential_message = (
    CREDENTIAL + user + device_9 + struct.pack(">Qqq", 3, T, T + 90 * DAY)
)

# A state signed by A at revision 2: no revocations, C added by A two
# hours before T (C's possession over its addition, A's endorsement over
# it and the time), B removed an hour before T.
c_root = key_root(C, C_ID)
add_message = ROOT_ADD + user + c_root
c_added_at = T - 7200
endorse_message = ROOT_ENDORSE + user + c_root + struct.pack(">q", c_added_at)
state_message = (
    STATE_V3
    + user
    + struct.pack(">Qq", 2, T)
    + struct.pack(">I", 0)
    + roots[0]
    + bytes([1])
    + c_root
    + roots[0]
    + struct.pack(">q", c_added_at)
    + bytes([1])
    + roots[1]
    + struct.pack(">q", T - 3600)
)

# A phrase root (seed 7, P7-02's) adding key root A to its identity.
phrase_user = ed_public(7)
phrase_add_message = ROOT_ADD + phrase_user + roots[0]
phrase_endorse_message = ROOT_ENDORSE + phrase_user + roots[0] + struct.pack(">q", T)

print(
    json.dumps(
        {
            "a": {"public_key": sec1(A).hex(), "credential_id": A_ID.hex()},
            "b": {"public_key": sec1(B).hex(), "credential_id": B_ID.hex()},
            "c": {"public_key": sec1(C).hex(), "credential_id": C_ID.hex()},
            "handle": HANDLE.hex(),
            "genesis_message": genesis_message.hex(),
            "genesis_signatures": [assert_(A, genesis_message), assert_(B, genesis_message)],
            "user": user.hex(),
            "device_9": device_9.hex(),
            "credential_message": credential_message.hex(),
            "credential_by_a": assert_(A, credential_message),
            "credential_by_b": assert_(B, credential_message),
            "add_c_message": add_message.hex(),
            "add_c_possession": assert_(C, add_message),
            "endorse_c_message": endorse_message.hex(),
            "endorse_c_by_a": assert_(A, endorse_message),
            "state_message": state_message.hex(),
            "state_by_a": assert_(A, state_message),
            "phrase_user": phrase_user.hex(),
            "phrase_add_a_message": phrase_add_message.hex(),
            "phrase_add_a_possession": assert_(A, phrase_add_message),
            "phrase_endorse_a_message": phrase_endorse_message.hex(),
            "phrase_endorse_a": ed(7).sign(phrase_endorse_message).hex(),
            # Refusals: the same credential, asserted without the PIN
            # (UP only), without a touch (UV only), and under another RP.
            "credential_by_a_up_only": assert_(A, credential_message, flags=0x01),
            "credential_by_a_uv_only": assert_(A, credential_message, flags=0x04),
            "credential_by_a_other_rp": {
                "authenticator_data": (
                    hashlib.sha256(b"server.spoolframe.app").digest()
                    + bytes([0x05])
                    + struct.pack(">I", 0)
                ).hex(),
                "signature": A.sign(
                    hashlib.sha256(b"server.spoolframe.app").digest()
                    + bytes([0x05])
                    + struct.pack(">I", 0)
                    + hashlib.sha256(credential_message).digest(),
                    ec.ECDSA(hashes.SHA256()),
                ).hex(),
            },
        },
        indent=2,
    )
)
