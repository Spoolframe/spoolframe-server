-- P7-04: a local account tied to a portable identity. The tie is a link
-- beside the account, never a move: sessions, passkeys, watch state and
-- every other row stay keyed on the local user. Severing it deletes this
-- row and nothing else. One account ties to at most one identity, and one
-- identity to at most one account on this server.
CREATE TABLE portable_ties (
    local_user_id TEXT PRIMARY KEY NOT NULL REFERENCES users (id),
    -- The PortableUserId, 64 lowercase hex digits.
    portable_user_id TEXT NOT NULL UNIQUE,
    -- Unix seconds.
    tied_at INTEGER NOT NULL,
    -- What signed the tie: 'root', or 'device' (a device key with its
    -- credential).
    proof_kind TEXT NOT NULL
);
