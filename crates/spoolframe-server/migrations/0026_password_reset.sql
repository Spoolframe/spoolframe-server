-- An admin resets a user's password to a one-time one.
--
-- How each session was signed in, so the reset ends only what rests on the
-- password: 'password', 'passkey', 'sso' or 'portable_credential'. A
-- session a device approval or a trusted device's resume made carries the
-- way its parent was signed in. NULL: made before this column, or from a
-- parent of unknown origin; a reset treats it as a password's.
ALTER TABLE sessions ADD COLUMN signed_in_with TEXT
    CHECK (signed_in_with IN ('password', 'passkey', 'sso', 'portable_credential'));

-- How the session that trusted a device was signed in: a resume from that
-- device carries it on. NULL as for sessions.
ALTER TABLE trusted_endpoints ADD COLUMN signed_in_with TEXT
    CHECK (signed_in_with IN ('password', 'passkey', 'sso', 'portable_credential'));

-- A session signed in with a one-time password an admin reset to: it may
-- only change the password, or sign out.
ALTER TABLE sessions ADD COLUMN password_change_only INTEGER NOT NULL DEFAULT 0
    CHECK (password_change_only IN (0, 1));

-- A password an admin reset to a one-time one: the user must choose a new
-- one when they next sign in with it, and it stops working at expires_at
-- (Unix seconds). Any password set the usual way clears both.
ALTER TABLE password_credentials ADD COLUMN must_change INTEGER NOT NULL DEFAULT 0
    CHECK (must_change IN (0, 1));
ALTER TABLE password_credentials ADD COLUMN expires_at INTEGER;
