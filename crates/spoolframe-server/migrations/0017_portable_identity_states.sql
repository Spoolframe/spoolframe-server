-- P7-02: the newest identity state this server has seen for each portable
-- user (ARCHITECTURE §48). A state is the user's root's signed list of
-- revoked device keys; a credential whose key the stored state revokes no
-- longer authenticates here. Only a higher revision replaces a row, so a
-- device carrying an older state cannot undo a revocation.
CREATE TABLE portable_identity_states (
    -- The PortableUserId, 64 lowercase hex digits.
    user_id TEXT PRIMARY KEY,
    -- The state's revision; what orders states.
    revision INTEGER NOT NULL,
    -- The whole signed state, as JSON in the protocol crate's serde form,
    -- so it can be handed on as it was signed.
    state TEXT NOT NULL,
    -- When this revision was first seen here, in Unix seconds.
    seen_at INTEGER NOT NULL
);
