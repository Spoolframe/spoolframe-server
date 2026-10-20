-- PX-37: optimized versions, converted copies the server keeps.
--
-- The admin's profiles, in the order the web UI lists them. A profile is
-- a video codec, a height and a bitrate (bits per second); its copies
-- carry every audio and text track, so one copy serves every language.
CREATE TABLE optimized_profiles (
    name TEXT PRIMARY KEY NOT NULL,
    video_codec TEXT NOT NULL,
    height INTEGER NOT NULL,
    max_bitrate INTEGER NOT NULL,
    position INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- The three the server ships with: few profiles mean more reuse.
INSERT INTO optimized_profiles (name, video_codec, height, max_bitrate, position, updated_at)
VALUES ('Mobile', 'h264', 1080, 4000000, 0, 0),
       ('TV', 'hevc', 1080, 12000000, 1, 0),
       ('Compact 4K', 'hevc', 2160, 20000000, 2, 0);

-- One copy of one media file, by an id of the server's own, which names
-- its file in `optimized.dir`. `target` is what it was made for, one
-- string to keep it unique: `profile:<name>`, or `ahead:<plan>` for a
-- binge-ahead copy made with a playback's own plan (no profile). The
-- video columns are the plan it is made with, whatever the target: the
-- planner matches on them, and they stay true after its profile is
-- edited. `source_stamp` is the media file's size and modification time
-- when it was asked for: a changed file needs a new copy.
--
-- `state` is `waiting`, `making`, `ready` or `failed`. An `outdated` copy
-- (its profile edited or deleted, its file changed) is never used again
-- and is evicted first; it no longer holds its place, so a new copy for
-- the same target may be made beside it.
CREATE TABLE optimized_copies (
    id TEXT PRIMARY KEY NOT NULL,
    media_id TEXT NOT NULL,
    media_path TEXT NOT NULL,
    source_stamp TEXT NOT NULL,
    target TEXT NOT NULL,
    video_codec TEXT NOT NULL,
    height INTEGER NOT NULL,
    max_bitrate INTEGER,
    tone_map INTEGER NOT NULL,
    state TEXT NOT NULL,
    size_bytes INTEGER,
    failure TEXT,
    outdated INTEGER NOT NULL DEFAULT 0,
    requested_at INTEGER NOT NULL,
    made_at INTEGER,
    last_used_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX optimized_copies_live ON optimized_copies (media_path, target)
    WHERE outdated = 0;
CREATE INDEX optimized_copies_media ON optimized_copies (media_id);
CREATE INDEX optimized_copies_target ON optimized_copies (target);
