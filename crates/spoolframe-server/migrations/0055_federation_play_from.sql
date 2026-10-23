-- P8-05: the server each user plays a title from, when the title is on
-- several (this one and paired servers, or several paired servers).
-- Kept by the title's external ids, as watch state's title key has them
-- (`tmdb://603`, an episode's `tmdb://1396#s1e2`), so every copy of the
-- title, here or on any paired server, finds the choice, and it outlives
-- a pair made again and an item given a new key. And by the title's
-- kind: TMDB numbers films and shows apart, so `tmdb://348` is a film
-- and a show, each with a choice of its own.
CREATE TABLE federation_play_from (
    -- A Principal, as watch_state's.
    principal TEXT NOT NULL,
    -- The title's MediaKind: `movie`, `show`, `episode`, ...
    kind TEXT NOT NULL,
    external_id TEXT NOT NULL,
    -- The paired server's ServerId, or '' for this server.
    server TEXT NOT NULL,
    -- Unix seconds: of two choices a title's ids hold, the newer is it.
    chosen_at INTEGER NOT NULL,
    PRIMARY KEY (principal, kind, external_id)
);
