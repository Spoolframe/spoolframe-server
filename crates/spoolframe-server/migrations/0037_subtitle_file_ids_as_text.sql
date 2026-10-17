-- PX-02, SubDL: a site's id of a file is text, not a number. OpenSubtitles'
-- are numbers; SubDL's are two joined by a dash, `3172974-3188291`. The two
-- tables that keep one are made again with `file_id TEXT`, their rows
-- copied, OpenSubtitles' numbers as their decimal text.
CREATE TABLE subtitle_downloads_new (
    id TEXT PRIMARY KEY NOT NULL,
    media_id TEXT NOT NULL,
    media_path TEXT NOT NULL,
    source TEXT NOT NULL,
    file_id TEXT NOT NULL,
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

-- By rowid, so downloads keep the order they were made in.
INSERT INTO subtitle_downloads_new
    (id, media_id, media_path, source, file_id, language, release, format,
     hearing_impaired, origin, hash_match, downloaded_by, downloaded_name, downloaded_at)
SELECT id, media_id, media_path, source, CAST(file_id AS TEXT), language, release, format,
       hearing_impaired, origin, hash_match, downloaded_by, downloaded_name, downloaded_at
FROM subtitle_downloads ORDER BY rowid;

DROP TABLE subtitle_downloads;
ALTER TABLE subtitle_downloads_new RENAME TO subtitle_downloads;
CREATE INDEX subtitle_downloads_media ON subtitle_downloads (media_id);

CREATE TABLE subtitle_rejections_new (
    media_id TEXT NOT NULL,
    source TEXT NOT NULL,
    file_id TEXT NOT NULL,
    rejected_by TEXT NOT NULL,
    rejected_at INTEGER NOT NULL,
    PRIMARY KEY (media_id, source, file_id)
);

INSERT INTO subtitle_rejections_new (media_id, source, file_id, rejected_by, rejected_at)
SELECT media_id, source, CAST(file_id AS TEXT), rejected_by, rejected_at FROM subtitle_rejections;

DROP TABLE subtitle_rejections;
ALTER TABLE subtitle_rejections_new RENAME TO subtitle_rejections;
