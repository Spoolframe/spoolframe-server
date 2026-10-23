-- P8-02, what a client that names the experimental `federation` feature
-- is shown of the paired servers' cache.

-- When this server first cached each item, Unix seconds: what a paired
-- server's Home row and the cache's recently added go by. Plex's own
-- `addedAt` is the origin's business and does not cross; an item first
-- cached by a whole library's first sync is as new as the rest of it.
-- 0 for the rows cached before this column.
ALTER TABLE federation_cache_items ADD COLUMN cached_at INTEGER NOT NULL DEFAULT 0;

CREATE INDEX federation_cache_items_recent
    ON federation_cache_items (origin, cached_at);
-- Every origin's at once: recently added across the paired servers.
CREATE INDEX federation_cache_items_recent_all
    ON federation_cache_items (cached_at)
    WHERE unavailable = 0 AND kind IN ('movie', 'show', 'album');

-- The paired servers whose own Home row a user turned off. Kept by
-- origin (the server's ServerId), as the cache is, so a pair made again
-- keeps the setting. Only the rows turned off are kept.
CREATE TABLE federation_home_rows_hidden (
    -- A Principal, as watch_state's.
    principal TEXT NOT NULL,
    origin TEXT NOT NULL,
    -- Unix seconds.
    hidden_at INTEGER NOT NULL,
    PRIMARY KEY (principal, origin)
);
