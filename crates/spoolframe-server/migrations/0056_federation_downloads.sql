-- P8-07: downloads of a paired server's items.

-- On the server a user is at home on (SB): its record of each download of
-- a paired server's (SA's) item, so DownloadStatus, CancelDownload and the
-- device's Downloads screen work as for SB's own. SA makes and serves the
-- file under its own id (`remote_id`); nothing of the file is here. The
-- device names `id`, SB's own.
CREATE TABLE federation_downloads (
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
    UNIQUE (principal, remote_id)
);

CREATE INDEX federation_downloads_origin ON federation_downloads (origin, library);

-- On SA: a device's download token (P8-07) reads this one download, by
-- this server's id. NULL for a play token, and for the token SB itself
-- prepares the download with.
ALTER TABLE federation_tokens ADD COLUMN download TEXT;
