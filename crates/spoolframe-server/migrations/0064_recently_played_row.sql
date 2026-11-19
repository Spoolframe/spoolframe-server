-- Home's "Recently played" row (feature `spoolframe:recently-played-row`).
--
-- Where a track's play was started (`StartPlayback.source`): 'album' (its
-- id a MediaId), 'playlist' (its id a playlist's), or 'queued' (no id),
-- added to the queue by hand. NULL for a play with none, every play from
-- before this, and a film's or an episode's: it shows its album.
ALTER TABLE plays ADD COLUMN source_kind TEXT;
ALTER TABLE plays ADD COLUMN source_id TEXT;
-- An episode's season, so the row can fold a season's plays without a
-- catalog call. NULL for every play from before this: the row looks the
-- season up for those.
ALTER TABLE plays ADD COLUMN season_id TEXT;

-- How each kind shows in a user's row, a JSON `RecentlyPlayedPreferences`,
-- as track_preferences keeps theirs: written whole, read over the
-- defaults. It changes only what the row shows, never what is counted.
CREATE TABLE recently_played_preferences (
    principal TEXT PRIMARY KEY NOT NULL,
    preferences TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
