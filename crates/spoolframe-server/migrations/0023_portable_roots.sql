-- P7-08: identities with more roots than the phrase root their id is.
-- Only identities this server knows (tied or granted) are kept, as their
-- states are: a stranger's would fill the disk.

-- An identity's genesis: its first roots, each signing it. Its hash is
-- the identity's id, so it proves itself; a phrase identity's has none
-- here (its one root is its id).
CREATE TABLE portable_geneses (
    -- The PortableUserId, 64 lowercase hex digits.
    user_id TEXT PRIMARY KEY,
    -- The genesis as JSON, in the protocol crate's serde form.
    genesis TEXT NOT NULL,
    -- When it was first seen here, in Unix seconds.
    seen_at INTEGER NOT NULL
);

-- Every valid state that names its signer or changes roots: the roots
-- are worked out from all of them (a removal by any root ever added
-- counts), not only from the newest, as revocations are.
CREATE TABLE portable_root_states (
    user_id TEXT NOT NULL,
    -- BLAKE3 of what was signed and the signature, hex: one row per
    -- distinct state.
    digest TEXT NOT NULL,
    revision INTEGER NOT NULL,
    -- The whole signed state, as JSON.
    state TEXT NOT NULL,
    PRIMARY KEY (user_id, digest)
);
