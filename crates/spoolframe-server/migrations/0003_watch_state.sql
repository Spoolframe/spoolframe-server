-- P1-14: each principal's own progress and watched state (ARCHITECTURE
-- §36). Movies and episodes only in Phase 1.

CREATE TABLE watch_state (
    -- A Principal, e.g. 'local:3'. Text so portable and remote users (§55)
    -- fit later without a migration of this column.
    principal TEXT NOT NULL,
    -- The item's MediaId as clients see it, e.g. 'plex:episode:208620'.
    media_id TEXT NOT NULL,
    -- For an episode, its show's MediaId: what next_up groups by.
    show_id TEXT,
    -- Milliseconds; NULL when playback never got anywhere.
    position_ms INTEGER,
    duration_ms INTEGER,
    watched INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (principal, media_id)
);

CREATE INDEX watch_state_recent ON watch_state (principal, updated_at);

-- The catalog's stable ids for each item that has a watch state: Plex's
-- guid and the agents' (imdb://, tmdb://, tvdb://). A ratingKey changes
-- when Plex rebuilds or re-matches a library; when an item turns up under
-- a new MediaId, its row is found here and re-linked to it (the ids move
-- with it through the cascade). P9's move to a native catalog re-links
-- the same way. One row per id: an id names one item.
CREATE TABLE watch_state_ids (
    principal TEXT NOT NULL,
    media_id TEXT NOT NULL,
    external_id TEXT NOT NULL,
    PRIMARY KEY (principal, external_id),
    FOREIGN KEY (principal, media_id) REFERENCES watch_state (principal, media_id)
        ON UPDATE CASCADE ON DELETE CASCADE
);
