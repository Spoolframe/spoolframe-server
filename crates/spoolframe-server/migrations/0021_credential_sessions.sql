-- P7-06, after review: a session a device credential opened ends when the
-- credential does. Each keeps the device key that signed in, so taking in
-- an identity state that revokes the key revokes its sessions, and the
-- credential's expiry, past which it is never renewed.

-- A tied account's session opened with a credential: whose key, and until
-- when. NULL for every other session.
ALTER TABLE sessions ADD COLUMN portable_user_id TEXT;
ALTER TABLE sessions ADD COLUMN portable_device_key TEXT;
ALTER TABLE sessions ADD COLUMN portable_expires_at INTEGER;

-- A portable user's session: the same. NULL only for rows from before
-- this, which then end on their own lifetime.
ALTER TABLE portable_sessions ADD COLUMN device_key TEXT;
ALTER TABLE portable_sessions ADD COLUMN credential_expires_at INTEGER;

-- A revoked grant is kept as a tombstone, so the invite it came by (or
-- any made before the revocation) cannot bring it back. A live grant has
-- none.
ALTER TABLE portable_grants ADD COLUMN revoked_at INTEGER;
