-- P2-08: a PIN on a profile, and on which of the user's devices it is
-- asked for.

-- Argon2id in PHC string format, as passwords are. Nobody can read it,
-- admins included.
CREATE TABLE pin_credentials (
    user_id INTEGER PRIMARY KEY REFERENCES users (id),
    phc TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- A row: the user's PIN is asked for on that device (its Iroh endpoint),
-- whoever set it and from where. No row: not asked for, the default. It
-- outlives the device's sessions, so signing out and in keeps it.
CREATE TABLE pin_required (
    user_id INTEGER NOT NULL REFERENCES users (id),
    endpoint_id TEXT NOT NULL REFERENCES devices (endpoint_id),
    set_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, endpoint_id)
);
