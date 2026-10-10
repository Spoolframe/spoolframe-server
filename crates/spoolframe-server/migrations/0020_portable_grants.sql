-- P7-06: portable users this server lets watch, and their sessions. A
-- grant is the whole of what the server keeps of someone whose account it
-- does not host (ARCHITECTURE §50, §51): their public id, what to call
-- them, and when. Never a credential, password or recovery. They are no
-- user here: nothing of theirs is in `users`, and the users list does not
-- show them.
CREATE TABLE portable_grants (
    -- The PortableUserId, 64 lowercase hex digits.
    portable_user_id TEXT PRIMARY KEY NOT NULL,
    -- What they asked to be called when the grant was made; shown to
    -- admins, and as their name in apps. NULL: their id's fingerprint.
    display_name TEXT,
    -- Unix seconds.
    granted_at INTEGER NOT NULL,
    -- The invite that made it (P7-05); NULL for one an admin made.
    invite_id TEXT
);

-- A portable user's sessions: as `sessions`' endpoint rows, but keyed on
-- the grant. Revoking the grant deletes them, so a session outlives its
-- grant by nothing.
CREATE TABLE portable_sessions (
    -- A hash of the token; the token itself is never stored.
    token_hash TEXT PRIMARY KEY NOT NULL,
    portable_user_id TEXT NOT NULL
        REFERENCES portable_grants (portable_user_id) ON DELETE CASCADE,
    -- Hex EndpointId: the session is usable only from it.
    endpoint_id TEXT NOT NULL,
    device_name TEXT,
    platform TEXT,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER
);

CREATE INDEX portable_sessions_by_user ON portable_sessions (portable_user_id);
