-- PX-38's downloads: which user prepared which file (the user's decision,
-- 2026-10-20), so a restart does not write a ready file again. Nothing
-- lists these for an admin: downloads are the user's business.
--
-- `id` is the download's, random (128 bits, hex), bound to its
-- `principal`; the same principal asking for the same `request` (its
-- media, source, tracks and device class, as JSON) gets the same id.
-- `content_key` names the file the request is written as, from what the
-- file is made of (the media file as it was, the tracks, the container,
-- the subtitles, the stored tracks it reads): two users' same request
-- share one file. `state` is that file's: waiting, converting, writing,
-- ready or failed, with `failure` saying why. Once ready, `file` is where
-- it is (the media file itself when it is sent as it is), with its
-- `size_bytes` and `sha256`. `last_read_at` is when it was last read, or
-- made ready: the file goes a day after it.
--
-- `carried` is the tracks the file carries, so `DownloadStatus` says what
-- `PrepareDownload` said: JSON `{"audio": [..], "subtitles": [..]}`, by
-- their place in the item's tracks, in the file's order. `stamp` is the
-- media file's size and modification time the content key was made from:
-- a file sent as it is (the media file itself) that no longer has it is
-- not served under the old size and SHA-256. Both change with
-- `content_key`, which is made of the same.
CREATE TABLE downloads (
    id TEXT PRIMARY KEY NOT NULL,
    principal TEXT NOT NULL,
    media_id TEXT NOT NULL,
    request TEXT NOT NULL,
    content_key TEXT NOT NULL,
    state TEXT NOT NULL,
    file TEXT,
    size_bytes INTEGER,
    sha256 TEXT,
    failure TEXT,
    created_at INTEGER NOT NULL,
    last_read_at INTEGER,
    carried TEXT NOT NULL,
    stamp TEXT NOT NULL
);

CREATE UNIQUE INDEX downloads_by_request ON downloads (principal, media_id, request);
CREATE INDEX downloads_by_content ON downloads (content_key);
