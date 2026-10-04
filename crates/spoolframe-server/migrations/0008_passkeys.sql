-- P3-05: passkeys (WebAuthn credentials) on existing accounts, and the
-- recent sign-in that adding or removing one needs (P3-03).

-- The WebAuthn user handle: what an authenticator keeps beside a passkey
-- and gives back at a sign-in without a username. Random, so it says
-- nothing about the user; one per user, so an authenticator holds one
-- passkey per account, not one per registration.
CREATE TABLE passkey_users (
    user_id INTEGER PRIMARY KEY REFERENCES users (id),
    -- A UUID.
    handle TEXT NOT NULL UNIQUE
);

CREATE TABLE passkeys (
    -- The credential id, base64url: one passkey belongs to one user.
    credential_id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users (id),
    -- What its user calls it ("MacBook", "YubiKey").
    name TEXT NOT NULL,
    -- webauthn-rs' credential as JSON: the public key, the signature
    -- counter and what else it verifies an assertion against.
    credential TEXT NOT NULL,
    -- The relying-party ID it was made under (the host of
    -- admin.public_url then); it works nowhere else.
    rp_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER
);

CREATE INDEX passkeys_by_user ON passkeys (user_id);

-- When the session's user last proved who they are in it: at sign-in, and
-- again when they confirm it for a sensitive change.
ALTER TABLE sessions ADD COLUMN authenticated_at INTEGER NOT NULL DEFAULT 0;
UPDATE sessions SET authenticated_at = created_at;
