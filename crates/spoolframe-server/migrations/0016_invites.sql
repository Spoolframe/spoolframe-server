-- P6-01: invites. A row is a code an admin made that lets whoever has it
-- make their own account, `uses` times, until `expires_at`, unless it is
-- revoked first. The code itself is never kept: only its BLAKE3 hash, as
-- for session tokens, so the admin sees it once, when it is made.
CREATE TABLE invites (
    -- A UUIDv7, as users' ids are.
    id TEXT PRIMARY KEY,
    -- What redeeming it does: 'create-user', the only kind made so far.
    -- Others ('portable-account', 'federation') arrive with P7 and P8.
    kind TEXT NOT NULL,
    capability_hash TEXT NOT NULL UNIQUE,
    -- The role the accounts it makes get: 'consumer' or 'admin'.
    role TEXT NOT NULL,
    -- Who it is for, as the admin wrote it; only admins see it.
    note TEXT,
    -- How many accounts it may make in all, and how many it still may.
    uses INTEGER NOT NULL CHECK (uses >= 1),
    uses_left INTEGER NOT NULL CHECK (uses_left >= 0 AND uses_left <= uses),
    -- Unix seconds, as every time here.
    expires_at INTEGER NOT NULL,
    -- Who made it, as the admin pages show it: the admin's username, or
    -- 'server CLI' for one made with `spoolframe-server invite create`.
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
