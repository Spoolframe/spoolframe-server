-- P3-20: users get UUIDv7 ids, in place of SQLite's rowids. Every server
-- had a user 1, and the numbers told how many users a server had and in
-- what order they came.
--
-- One transaction, as every migration is: a failure part-way leaves the
-- database as it was. The server copies the database to
-- spoolframe.db.pre-0012 before this runs (store::open), and renames the
-- secrets named by user id after it (from legacy_user_ids, below).
--
-- Foreign keys stay on (the pool sets them, and they cannot be turned off
-- inside a transaction), so the tables are rebuilt in an order that never
-- leaves a row without its user: each child's rows are copied aside with
-- their new ids, the children are dropped, then `users` is replaced, then
-- the children are made again from the copies.

-- Each user's new id. UUIDv7 (RFC 9562): 48 bits of Unix milliseconds,
-- then the version, then random bits with the variant. The milliseconds
-- are the user's creation second plus their place among the users made in
-- that second, so the ids sort as the users were made, as the rowids did.
-- Kept, not dropped: the server renames `secrets/plex-account-<old id>`
-- from it after this commits, and again at a start after a crash between
-- the two.
CREATE TABLE legacy_user_ids (
    old_id INTEGER PRIMARY KEY,
    new_id TEXT UNIQUE,
    -- Scratch for making new_id: the timestamp and the random hex, each
    -- drawn once (a random() in an expression used twice is drawn twice).
    ms INTEGER,
    rand TEXT
);

INSERT INTO legacy_user_ids (old_id, ms, rand)
SELECT id,
       created_at * 1000
           + (row_number() OVER (PARTITION BY created_at ORDER BY id) - 1),
       lower(hex(randomblob(10)))
FROM users;

UPDATE legacy_user_ids SET new_id =
    substr(printf('%012x', ms), 1, 8) || '-'
    || substr(printf('%012x', ms), 9, 4) || '-'
    || '7' || substr(rand, 1, 3) || '-'
    -- The variant: 10xx, one of 8, 9, a, b.
    || substr('89ab', 1 + (unicode(substr(rand, 19, 1)) % 4), 1)
    || substr(rand, 4, 3) || '-'
    || substr(rand, 7, 12);

-- The children's rows, with their users' new ids. A row whose user does
-- not exist cannot be copied: the foreign keys below refuse it, and the
-- migration fails whole rather than lose it quietly.
CREATE TEMP TABLE m_password_credentials AS
SELECT l.new_id AS user_id, c.phc, c.updated_at
FROM password_credentials c LEFT JOIN legacy_user_ids l ON l.old_id = c.user_id;

CREATE TEMP TABLE m_sessions AS
SELECT s.token_hash, l.new_id AS user_id, s.kind, s.endpoint_id, s.created_at,
       s.expires_at, s.revoked_at, s.last_used_at, s.authenticated_at
FROM sessions s LEFT JOIN legacy_user_ids l ON l.old_id = s.user_id;

CREATE TEMP TABLE m_pin_credentials AS
SELECT l.new_id AS user_id, c.phc, c.updated_at, c.digits
FROM pin_credentials c LEFT JOIN legacy_user_ids l ON l.old_id = c.user_id;

CREATE TEMP TABLE m_pin_required AS
SELECT l.new_id AS user_id, r.endpoint_id, r.set_at
FROM pin_required r LEFT JOIN legacy_user_ids l ON l.old_id = r.user_id;

CREATE TEMP TABLE m_passkey_users AS
SELECT l.new_id AS user_id, p.handle
FROM passkey_users p LEFT JOIN legacy_user_ids l ON l.old_id = p.user_id;

CREATE TEMP TABLE m_passkeys AS
SELECT p.credential_id, l.new_id AS user_id, p.name, p.credential, p.rp_id,
       p.created_at, p.last_used_at
FROM passkeys p LEFT JOIN legacy_user_ids l ON l.old_id = p.user_id;

CREATE TEMP TABLE m_external_identities AS
SELECT l.new_id AS user_id, i.issuer, i.subject, i.display, i.linked_at,
       i.last_used_at
FROM external_identities i LEFT JOIN legacy_user_ids l ON l.old_id = i.user_id;

DROP INDEX sessions_by_user;
DROP INDEX passkeys_by_user;
DROP TABLE password_credentials;
DROP TABLE sessions;
DROP TABLE pin_credentials;
DROP TABLE pin_required;
DROP TABLE passkey_users;
DROP TABLE passkeys;
DROP TABLE external_identities;

CREATE TABLE users_new (
    -- A UUIDv7, lowercase and hyphenated. The length check keeps a number
    -- out: TEXT affinity would store one as its digits.
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    -- Lowercased, so names are unique without case.
    username TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    disabled_at INTEGER,
    -- What clients show for a user ("Who's watching"); the username when
    -- NULL.
    display_name TEXT,
    role TEXT NOT NULL DEFAULT 'consumer' CHECK (role IN ('admin', 'consumer'))
);

INSERT INTO users_new (id, username, created_at, disabled_at, display_name, role)
SELECT l.new_id, u.username, u.created_at, u.disabled_at, u.display_name, u.role
FROM users u JOIN legacy_user_ids l ON l.old_id = u.id;

DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

-- The children as 0001, 0007, 0008 and 0009 made them, with the columns
-- later migrations added, and user_id now the user's UUID.
CREATE TABLE password_credentials (
    user_id TEXT PRIMARY KEY NOT NULL REFERENCES users (id),
    -- Argon2id in PHC string format, parameters included.
    phc TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE sessions (
    -- A hash of the token; the token itself is never stored.
    token_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users (id),
    -- 'endpoint': an Iroh client, bound to endpoint_id (P1-12).
    -- 'api': the admin API (CLI, and the web UI with P1-17).
    kind TEXT NOT NULL CHECK (kind IN ('endpoint', 'api')),
    endpoint_id TEXT REFERENCES devices (endpoint_id),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER,
    -- When the session was last used (P1-12).
    last_used_at INTEGER,
    -- When the session's user last proved who they are in it (P3-05).
    authenticated_at INTEGER NOT NULL DEFAULT 0,
    CHECK ((kind = 'endpoint') = (endpoint_id IS NOT NULL))
);

CREATE INDEX sessions_by_user ON sessions (user_id);

CREATE TABLE pin_credentials (
    user_id TEXT PRIMARY KEY NOT NULL REFERENCES users (id),
    -- Argon2id in PHC string format, as passwords are.
    phc TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    digits INTEGER CHECK (digits BETWEEN 4 AND 8)
);

CREATE TABLE pin_required (
    user_id TEXT NOT NULL REFERENCES users (id),
    endpoint_id TEXT NOT NULL REFERENCES devices (endpoint_id),
    set_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, endpoint_id)
);

-- The WebAuthn user handle stays what it was: a random UUID of its own,
-- so a passkey says nothing about its account's id.
CREATE TABLE passkey_users (
    user_id TEXT PRIMARY KEY NOT NULL REFERENCES users (id),
    handle TEXT NOT NULL UNIQUE
);

CREATE TABLE passkeys (
    -- The credential id, base64url: one passkey belongs to one user.
    credential_id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users (id),
    name TEXT NOT NULL,
    -- webauthn-rs' credential as JSON.
    credential TEXT NOT NULL,
    rp_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER
);

CREATE INDEX passkeys_by_user ON passkeys (user_id);

CREATE TABLE external_identities (
    user_id TEXT NOT NULL REFERENCES users (id),
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    display TEXT,
    linked_at INTEGER NOT NULL,
    last_used_at INTEGER,
    PRIMARY KEY (issuer, subject),
    UNIQUE (user_id, issuer)
);

INSERT INTO password_credentials SELECT user_id, phc, updated_at
FROM m_password_credentials;
INSERT INTO sessions (token_hash, user_id, kind, endpoint_id, created_at,
                      expires_at, revoked_at, last_used_at, authenticated_at)
SELECT token_hash, user_id, kind, endpoint_id, created_at, expires_at,
       revoked_at, last_used_at, authenticated_at
FROM m_sessions;
INSERT INTO pin_credentials (user_id, phc, updated_at, digits)
SELECT user_id, phc, updated_at, digits FROM m_pin_credentials;
INSERT INTO pin_required (user_id, endpoint_id, set_at)
SELECT user_id, endpoint_id, set_at FROM m_pin_required;
INSERT INTO passkey_users (user_id, handle) SELECT user_id, handle FROM m_passkey_users;
INSERT INTO passkeys (credential_id, user_id, name, credential, rp_id,
                      created_at, last_used_at)
SELECT credential_id, user_id, name, credential, rp_id, created_at, last_used_at
FROM m_passkeys;
INSERT INTO external_identities (user_id, issuer, subject, display, linked_at,
                                 last_used_at)
SELECT user_id, issuer, subject, display, linked_at, last_used_at
FROM m_external_identities;

DROP TABLE m_password_credentials;
DROP TABLE m_sessions;
DROP TABLE m_pin_credentials;
DROP TABLE m_pin_required;
DROP TABLE m_passkey_users;
DROP TABLE m_passkeys;
DROP TABLE m_external_identities;

-- State keyed on a principal: 'local:<old id>' becomes 'local:<uuid>'.
-- watch_state_ids and continue_hidden_ids follow their parents' rows
-- through ON UPDATE CASCADE.
UPDATE watch_state SET principal = 'local:' || (
    SELECT new_id FROM legacy_user_ids WHERE 'local:' || old_id = principal
) WHERE principal IN (SELECT 'local:' || old_id FROM legacy_user_ids);
UPDATE continue_hidden SET principal = 'local:' || (
    SELECT new_id FROM legacy_user_ids WHERE 'local:' || old_id = principal
) WHERE principal IN (SELECT 'local:' || old_id FROM legacy_user_ids);
UPDATE track_preferences SET principal = 'local:' || (
    SELECT new_id FROM legacy_user_ids WHERE 'local:' || old_id = principal
) WHERE principal IN (SELECT 'local:' || old_id FROM legacy_user_ids);
UPDATE track_overrides SET principal = 'local:' || (
    SELECT new_id FROM legacy_user_ids WHERE 'local:' || old_id = principal
) WHERE principal IN (SELECT 'local:' || old_id FROM legacy_user_ids);

-- A 'local:<number>' left now belongs to no user (none is ever deleted,
-- so there should be none): nobody can reach it again, and a later user
-- must not inherit it.
DELETE FROM watch_state_ids WHERE principal GLOB 'local:[0-9]*'
    AND substr(principal, 7) NOT GLOB '*[^0-9]*';
DELETE FROM watch_state WHERE principal GLOB 'local:[0-9]*'
    AND substr(principal, 7) NOT GLOB '*[^0-9]*';
DELETE FROM continue_hidden_ids WHERE principal GLOB 'local:[0-9]*'
    AND substr(principal, 7) NOT GLOB '*[^0-9]*';
DELETE FROM continue_hidden WHERE principal GLOB 'local:[0-9]*'
    AND substr(principal, 7) NOT GLOB '*[^0-9]*';
DELETE FROM track_preferences WHERE principal GLOB 'local:[0-9]*'
    AND substr(principal, 7) NOT GLOB '*[^0-9]*';
DELETE FROM track_overrides WHERE principal GLOB 'local:[0-9]*'
    AND substr(principal, 7) NOT GLOB '*[^0-9]*';

-- Only the mapping is kept.
CREATE TABLE legacy_user_ids_kept (
    old_id INTEGER PRIMARY KEY,
    new_id TEXT NOT NULL UNIQUE
);
INSERT INTO legacy_user_ids_kept SELECT old_id, new_id FROM legacy_user_ids;
DROP TABLE legacy_user_ids;
ALTER TABLE legacy_user_ids_kept RENAME TO legacy_user_ids;
