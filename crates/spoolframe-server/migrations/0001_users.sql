-- Local users, their password credentials, devices and sessions
-- (ARCHITECTURE §21). A credential is not part of the user: each login
-- method gets its own table, and all of them lead to a users.id (§24B).
-- Times are Unix seconds.

CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    -- Lowercased, so names are unique without case.
    username TEXT NOT NULL UNIQUE,
    is_admin INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    disabled_at INTEGER
);

CREATE TABLE password_credentials (
    user_id INTEGER PRIMARY KEY REFERENCES users (id),
    -- Argon2id in PHC string format, parameters included.
    phc TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE devices (
    -- Hex EndpointId.
    endpoint_id TEXT PRIMARY KEY,
    display_name TEXT,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL
);

CREATE TABLE sessions (
    -- A hash of the token; the token itself is never stored.
    token_hash TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users (id),
    -- 'endpoint': an Iroh client, bound to endpoint_id (P1-12).
    -- 'api': the admin API (CLI, and the web UI with P1-17).
    kind TEXT NOT NULL CHECK (kind IN ('endpoint', 'api')),
    endpoint_id TEXT REFERENCES devices (endpoint_id),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER,
    CHECK ((kind = 'endpoint') = (endpoint_id IS NOT NULL))
);

CREATE INDEX sessions_by_user ON sessions (user_id);
