-- PX-37's track store (the user's decision, 2026-10-20), with PX-38's
-- copies for downloads in original quality.
--
-- 0.13.0 kept each copy as one whole file. Now each converted stream is a
-- file of its own, a "track", and a copy is the source plus its tracks,
-- muxed when it is read. The whole-file copies are a cache: their rows
-- are dropped here, and the pool removes their files at its next start.
DROP TABLE optimized_copies;

-- One copy of one media file for one target, by an id of the server's
-- own. It holds no file: its tracks do. `target` is what it was made
-- for, one string to keep it unique: `profile:<name>`, `ahead:<plan>` for
-- a binge-ahead copy made with a playback's own plan, or
-- `original:<class>:<video>` for a download in original quality (PX-38),
-- which converts only what the device class cannot play. The video
-- columns are the plan it is made with: the planner matches on them, and
-- they stay true after its profile is edited. `video_copied` is 1 when
-- its video is the file's own (no video track is stored). `keep_audio` is
-- the audio codecs it takes from the file as they are, comma-separated as
-- the protocol spells them; NULL for PX-37's rule (AAC, AC-3 and E-AC-3).
-- `source_stamp` is the media file's size and modification time when it
-- was asked for.
--
-- Its state is its tracks': `failure` is set only when no track could be
-- asked for (the media file could not be read). An `outdated` copy (its
-- profile edited or deleted, its file changed, deleted while read) is
-- never used again; it no longer holds its place, so a new copy for the
-- same target may be asked for beside it.
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
    video_copied INTEGER NOT NULL DEFAULT 0,
    keep_audio TEXT,
    failure TEXT,
    outdated INTEGER NOT NULL DEFAULT 0,
    requested_at INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX optimized_copies_live ON optimized_copies (media_path, target)
    WHERE outdated = 0;
CREATE INDEX optimized_copies_media ON optimized_copies (media_id);
CREATE INDEX optimized_copies_target ON optimized_copies (target);

-- One converted stream of one media file, a file of its own in
-- `optimized.dir` named by its id. `recipe` says what it is, one string
-- to keep it unique: `video:<codec>:<height>:<bitrate|auto>:<sdr|same>`,
-- or `audio:<stream index>:<codec>:<channels>:<bitrate>`. One track serves
-- every copy that needs it. `state` is `waiting`, `making`, `ready` or
-- `failed`. An `outdated` track (its file changed, its profile edited and
-- no other copy using it, its copies deleted) is removed first.
CREATE TABLE optimized_tracks (
    id TEXT PRIMARY KEY NOT NULL,
    media_path TEXT NOT NULL,
    source_stamp TEXT NOT NULL,
    recipe TEXT NOT NULL,
    state TEXT NOT NULL,
    size_bytes INTEGER,
    failure TEXT,
    outdated INTEGER NOT NULL DEFAULT 0,
    requested_at INTEGER NOT NULL,
    made_at INTEGER,
    last_used_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX optimized_tracks_live ON optimized_tracks (media_path, recipe)
    WHERE outdated = 0;

-- Which tracks each copy is made of, in the order it asked for them
-- (`position`): its video first, then its audio by stream.
CREATE TABLE optimized_copy_tracks (
    copy_id TEXT NOT NULL,
    track_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY (copy_id, track_id)
);

CREATE INDEX optimized_copy_tracks_track ON optimized_copy_tracks (track_id);

-- An admin's on-demand request (the user's decision, 2026-10-20): copies
-- in `profile` of every film or episode `scope_id` holds, an item, a
-- season, a show or a library by `scope_kind`. It is a kept rule: what
-- the scope gains later is asked for too. One request per scope and
-- profile. A `pinned` request's copies are never evicted; it is refused
-- when `estimated_bytes`, beside the other pinned requests', would pass
-- `optimized.max_size_mb`. `created_by` is the admin's user id.
CREATE TABLE optimized_requests (
    id TEXT PRIMARY KEY NOT NULL,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    profile TEXT NOT NULL,
    pinned INTEGER NOT NULL DEFAULT 0,
    estimated_bytes INTEGER NOT NULL DEFAULT 0,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX optimized_requests_scope
    ON optimized_requests (scope_kind, scope_id, profile);

-- The films and episodes a request covers, and the copy made for each:
-- `copy_id` is NULL until it is asked for, and again once that copy is
-- gone (evicted, deleted). `asked` is 1 once the pool was asked for it,
-- so an evicted copy is not asked for again until its profile changes;
-- `failure` says why it could not be asked for (its file unreadable).
-- `round` goes up each time its profile changes: an ask begun before
-- that is not counted, so the item is asked for again.
CREATE TABLE optimized_request_items (
    request_id TEXT NOT NULL,
    media_id TEXT NOT NULL,
    copy_id TEXT,
    asked INTEGER NOT NULL DEFAULT 0,
    round INTEGER NOT NULL DEFAULT 0,
    failure TEXT,
    PRIMARY KEY (request_id, media_id)
);

CREATE INDEX optimized_request_items_copy ON optimized_request_items (copy_id);
