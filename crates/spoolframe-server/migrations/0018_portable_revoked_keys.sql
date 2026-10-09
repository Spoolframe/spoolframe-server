-- P7-02, follow-up: revocation only grows. A credential is refused when
-- its device key is in the union of every valid identity state seen for
-- its user, older revisions included, not only in the newest state, so a
-- later state signed without knowledge of a revocation cannot undo it.
-- A row is dropped once its revocation has lapsed: 90 days and 5 minutes
-- after `revoked_at`, when no credential it could stop still holds.
CREATE TABLE portable_revoked_keys (
    -- The PortableUserId, 64 lowercase hex digits.
    user_id TEXT NOT NULL,
    -- The revoked device key, 64 lowercase hex digits.
    device_key TEXT NOT NULL,
    -- When the root revoked it, in Unix seconds: the earliest any state
    -- seen here says.
    revoked_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, device_key)
);

-- States kept by 0017 are in the v1 layout, which has no revocation
-- times and no longer verifies or reads. None were ever sent by a
-- client; a device brings its state again.
DELETE FROM portable_identity_states;
