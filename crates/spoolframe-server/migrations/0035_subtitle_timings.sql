-- PX-03: how an external text subtitle is timed, the same for every user.
-- One row per subtitle file, by its path, made when someone first syncs it
-- or sets its offset. `stamp` is the file's size and modification time
-- then: a file edited since is served as it is, and its row means nothing
-- until it is synced or offset again.
--
-- `state` is the sync's: 'original' (never synced, or put back), 'syncing',
-- 'synced', 'uncertain' (scored too low; not used), 'failed' (`failure`
-- says why). `serving_synced` says which timing is served, which while a
-- sync runs is what it was before. The sync's findings (`sync_*`) are for
-- 'synced' and 'uncertain'. The manual offset is on top, set by
-- `offset_by` (a principal) as `offset_name` (their display name then) at
-- `offset_at`.
CREATE TABLE subtitle_timings (
    sidecar TEXT PRIMARY KEY NOT NULL,
    media_id TEXT NOT NULL,
    stamp TEXT NOT NULL,
    state TEXT NOT NULL,
    serving_synced INTEGER NOT NULL DEFAULT 0,
    sync_offset_ms INTEGER,
    sync_factor REAL,
    sync_splits INTEGER,
    sync_score REAL,
    sync_reference TEXT,
    failure TEXT,
    offset_ms INTEGER NOT NULL DEFAULT 0,
    offset_by TEXT,
    offset_name TEXT,
    offset_at INTEGER,
    updated_at INTEGER NOT NULL
);

CREATE INDEX subtitle_timings_media ON subtitle_timings (media_id);
