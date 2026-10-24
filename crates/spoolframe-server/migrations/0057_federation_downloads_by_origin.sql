-- P8-06, review F8-13: a paired server's id of a download names it only
-- on that server. Unique by (principal, remote_id), the same id from two
-- origins was one row, the second's preparation taking the first's over:
-- its origin, library and item. Made again unique by (principal, origin,
-- remote_id), its rows copied as they are: none can collide under the
-- wider key.
CREATE TABLE federation_downloads_new (
    id TEXT PRIMARY KEY NOT NULL,
    -- A Principal: the local user whose it is.
    principal TEXT NOT NULL,
    -- The paired server's ServerId, and the library (its own id there) the
    -- item is in: a library no longer shared, or the pair gone, makes the
    -- download no longer available.
    origin TEXT NOT NULL,
    library TEXT NOT NULL,
    -- The item, by its `fed:` id here.
    item TEXT NOT NULL,
    -- SA's id of the download: the same each time the same is prepared.
    remote_id TEXT NOT NULL,
    -- The wire's DownloadState and DownloadFile, as JSON, as SA last told
    -- them.
    state TEXT NOT NULL,
    file TEXT,
    expected_size INTEGER,
    -- Unix seconds.
    created_at INTEGER NOT NULL,
    -- When SA last told its state, or it was prepared: a failed one is
    -- forgotten a while after.
    updated_at INTEGER NOT NULL,
    -- When the item stopped being shared, or the pair ended.
    unavailable_at INTEGER,
    UNIQUE (principal, origin, remote_id)
);

INSERT INTO federation_downloads_new
    (id, principal, origin, library, item, remote_id, state, file, expected_size,
     created_at, updated_at, unavailable_at)
SELECT id, principal, origin, library, item, remote_id, state, file, expected_size,
       created_at, updated_at, unavailable_at
FROM federation_downloads ORDER BY rowid;

DROP TABLE federation_downloads;
ALTER TABLE federation_downloads_new RENAME TO federation_downloads;
CREATE INDEX federation_downloads_origin ON federation_downloads (origin, library);
