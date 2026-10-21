-- PX-51: when each track was last liked or unliked, so a change made on a
-- device with no server wins only when it is newer (SyncOfflinePlays).
-- Liked's entries keep when a track was added; an unlike leaves no entry,
-- so its time is kept here. Written by every like and unlike from now on;
-- a track with no row is judged by its entry's added_at, or is newer
-- than anything when it is not liked.
--
-- Plays made offline are recorded in `plays` as they are, closed, with
-- path 'offline' and playback_id 'offline:<the device's Unix seconds>'.
CREATE TABLE liked_changes (
    -- A Principal, e.g. 'local:<uuid>': Liked's owner.
    principal TEXT NOT NULL,
    -- The track's MediaId as clients see it.
    media_id TEXT NOT NULL,
    liked INTEGER NOT NULL,
    -- Unix seconds, by the server's clock (an offline change's moved to
    -- it).
    at INTEGER NOT NULL,
    PRIMARY KEY (principal, media_id)
);
