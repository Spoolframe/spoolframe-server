-- P8-01–P8-04: what a paired server's own page on the Federation page
-- says (the web UI's design boards).

-- When the pair was last seen connected, Unix seconds, written at most a
-- minute apart while it is: "offline since" this, once it is not.
ALTER TABLE federation_peers ADD COLUMN last_seen INTEGER;

-- When the pair's key last said hello under another server id, Unix
-- seconds: refused every time (PX-14), and the page says so, with
-- Unpair the only way on.
ALTER TABLE federation_peers ADD COLUMN identity_refused_at INTEGER;

-- How many whole syncs of a library brought a change: its revision as
-- this server holds it, for the page to name. The cursor is opaque.
ALTER TABLE federation_cache_libraries ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;

-- The plays across a pair a day at a time, as federation_plays counts
-- them in all: the page shows the last week's. Days are UTC, as Unix
-- days (seconds / 86400). Kept a week, then pruned.
CREATE TABLE federation_play_days (
    peer TEXT NOT NULL REFERENCES federation_peers (id),
    side TEXT NOT NULL,
    day INTEGER NOT NULL,
    plays INTEGER NOT NULL DEFAULT 0,
    proxied INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (peer, side, day)
);

-- A library a paired server stopped sharing with this one, as the
-- replica dropped its cache: the page says so for a week. Shared again,
-- its row goes.
CREATE TABLE federation_withdrawn (
    origin TEXT NOT NULL,
    library TEXT NOT NULL,
    peer TEXT NOT NULL REFERENCES federation_peers (id),
    title TEXT NOT NULL,
    withdrawn_at INTEGER NOT NULL,
    PRIMARY KEY (origin, library)
);
