-- P8-03: the libraries this server shares with a paired server. Grants
-- are server-wide: every user of the paired server browses and watches a
-- library shared with it, and there is no grant per remote user. One
-- permission per library: shared is browse and watch.
--
-- Revoking a grant, or unpairing (which revokes every grant of the pair),
-- sets `revoked_at` and keeps the row, as a pair's row is kept. Sharing
-- the library again makes a new row.
CREATE TABLE federation_grants (
    -- A UUIDv7, as text.
    id TEXT PRIMARY KEY NOT NULL,
    peer TEXT NOT NULL REFERENCES federation_peers (id),
    -- The library's LibraryId, as clients know it (`plex:2`).
    library TEXT NOT NULL,
    -- The admin who shared it, by username, or 'server CLI'.
    granted_by TEXT NOT NULL,
    -- Unix seconds.
    granted_at INTEGER NOT NULL,
    revoked_at INTEGER
);

-- One live grant per pair and library.
CREATE UNIQUE INDEX federation_grants_live
    ON federation_grants (peer, library) WHERE revoked_at IS NULL;
