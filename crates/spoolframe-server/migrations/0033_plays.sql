-- PX-52: every play, one row each, tracks included. Append-only: a row is
-- opened when a playback starts, moved on by its progress reports, and
-- closed when it stops, ends or expires; a closed row is never rewritten.
-- Only a re-link (below) moves a row's key, and a user's leaving deletes
-- their rows.
CREATE TABLE plays (
    id INTEGER PRIMARY KEY,
    -- A Principal, e.g. 'local:<uuid>', as watch_state's.
    principal TEXT NOT NULL,
    -- The item's MediaId as clients see it.
    media_id TEXT NOT NULL,
    -- 'movie', 'episode' or 'track'.
    kind TEXT NOT NULL,
    -- Where the kind has them: an episode's show, a track's album and
    -- artist; so history can be read by show, album or artist without a
    -- catalog call.
    show_id TEXT,
    album_id TEXT,
    artist_id TEXT,
    -- Unix seconds. ended_at is the last progress report while open.
    started_at INTEGER NOT NULL,
    ended_at INTEGER NOT NULL,
    -- Time actually played, not the position reached: a seek adds nothing.
    played_ms INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER,
    -- Whether it counts as a play (spoolframe_policy::plays::counts).
    counted INTEGER NOT NULL DEFAULT 0,
    -- 0 while its playback is open.
    closed INTEGER NOT NULL DEFAULT 0,
    -- The device: its endpoint id, and the name it gave.
    endpoint_id TEXT,
    device_name TEXT,
    -- 'iroh', or 'http' once a player fetched it from the HTTP playback
    -- port (PX-49).
    path TEXT NOT NULL DEFAULT 'iroh',
    -- The playback's id, as playback debug and the log name it (not its
    -- handle, which is a secret).
    playback_id TEXT
);

-- A principal's counted plays, by item: Recently played's page and count
-- and the play counts of a page of items read it alone (each item's
-- latest end is in it), and a user's leaving deletes by its principal.
CREATE INDEX plays_counted ON plays (principal, counted, media_id, ended_at);
-- The plays of an item, anyone's or one principal's.
CREATE INDEX plays_of_item ON plays (media_id, principal);

-- The catalog's stable ids of each item with plays, as watch_state_ids:
-- when an item turns up under a new MediaId, its plays are found here and
-- re-linked to it. No foreign key: an item has many plays.
CREATE TABLE play_ids (
    principal TEXT NOT NULL,
    media_id TEXT NOT NULL,
    external_id TEXT NOT NULL,
    PRIMARY KEY (principal, external_id)
);

CREATE INDEX play_ids_item ON play_ids (principal, media_id);
