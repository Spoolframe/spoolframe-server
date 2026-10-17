-- PX-02: subtitles downloaded from OpenSubtitles, for everyone on the
-- server. One row per download, by an id of the server's own, which names
-- its file in state_dir/subtitles/downloaded. `media_path` is the media
-- file it was downloaded for: the track is the file's while it is there.
-- `file_id` is the site's; one result is downloaded once per item.
-- `downloaded_by` is a principal, `downloaded_name` its display name then.
CREATE TABLE subtitle_downloads (
    id TEXT PRIMARY KEY NOT NULL,
    media_id TEXT NOT NULL,
    media_path TEXT NOT NULL,
    source TEXT NOT NULL,
    file_id INTEGER NOT NULL,
    language TEXT NOT NULL,
    release TEXT NOT NULL,
    format TEXT NOT NULL,
    hearing_impaired INTEGER NOT NULL,
    origin TEXT NOT NULL,
    hash_match INTEGER NOT NULL,
    downloaded_by TEXT NOT NULL,
    downloaded_name TEXT NOT NULL,
    downloaded_at INTEGER NOT NULL,
    UNIQUE (media_id, source, file_id)
);

CREATE INDEX subtitle_downloads_media ON subtitle_downloads (media_id);

-- Every download a user made, kept a day past it for their daily limit:
-- removing a track does not give the download back.
CREATE TABLE subtitle_download_log (
    principal TEXT NOT NULL,
    at INTEGER NOT NULL
);

CREATE INDEX subtitle_download_log_principal ON subtitle_download_log (principal, at);

-- A result removed as bad: listed last, marked, in the item's searches.
CREATE TABLE subtitle_rejections (
    media_id TEXT NOT NULL,
    source TEXT NOT NULL,
    file_id INTEGER NOT NULL,
    rejected_by TEXT NOT NULL,
    rejected_at INTEGER NOT NULL,
    PRIMARY KEY (media_id, source, file_id)
);

-- The users an admin took downloading from. Everyone else may download.
CREATE TABLE subtitle_download_denied (
    principal TEXT PRIMARY KEY NOT NULL,
    denied_by TEXT NOT NULL,
    denied_at INTEGER NOT NULL
);
