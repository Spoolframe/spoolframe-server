-- P7-09: a local account ties to any number of portable identities; an
-- identity still ties to at most one account here. `local_user_id` stops
-- being the key, `portable_user_id` stays UNIQUE, and `last_used_at` says
-- when an identity last signed in through its tie (P7-06).
--
-- SQLite cannot drop a primary key in place, so the table is made again
-- and every tie copied across. Nothing references `portable_ties`, so it
-- can be dropped with foreign keys on.
CREATE TABLE portable_ties_0028 (
    -- The PortableUserId, 64 lowercase hex digits. One account per
    -- identity on this server.
    portable_user_id TEXT PRIMARY KEY NOT NULL,
    local_user_id TEXT NOT NULL REFERENCES users (id),
    -- Unix seconds.
    tied_at INTEGER NOT NULL,
    -- What signed the tie: 'root', or 'device' (a device key with its
    -- credential).
    proof_kind TEXT NOT NULL,
    -- Unix seconds of the last sign-in with the identity's credential that
    -- landed in this account; NULL until one does.
    last_used_at INTEGER
);

INSERT INTO portable_ties_0028 (portable_user_id, local_user_id, tied_at, proof_kind)
SELECT portable_user_id, local_user_id, tied_at, proof_kind FROM portable_ties;

DROP TABLE portable_ties;
ALTER TABLE portable_ties_0028 RENAME TO portable_ties;

-- An account's ties, read for WhoAmI and the user list.
CREATE INDEX portable_ties_by_user ON portable_ties (local_user_id, tied_at);
