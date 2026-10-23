-- P8-02, on the server a library is shared with: its cache of what each
-- paired server shares, so its users browse it here, also while that
-- server is offline. Served under `fed:` ids, by the origin's server id
-- (not its key, so a rotated key keeps every id) and the origin's own id.
--
-- Rows are keyed by origin, not by pair row: a pair made again with the
-- same server fills the same rows, and the ids its users' watch state,
-- playlists and likes hold stay its items'.

-- A library a paired server shares with this one, as it last said.
CREATE TABLE federation_cache_libraries (
    -- The origin's ServerId.
    origin TEXT NOT NULL,
    -- Its id on the origin, as `plex:1`.
    library TEXT NOT NULL,
    -- The pair row it came over.
    peer TEXT NOT NULL REFERENCES federation_peers (id),
    title TEXT NOT NULL,
    -- 'movies', 'shows' or 'music'.
    kind TEXT NOT NULL,
    -- The cursor of the last whole sync; NULL until one is whole.
    cursor TEXT,
    -- Unix seconds of the last whole sync.
    synced_at INTEGER,
    PRIMARY KEY (origin, library)
);

-- Each item cached, as the origin sent it.
CREATE TABLE federation_cache_items (
    origin TEXT NOT NULL,
    -- Its id on the origin, as `plex:movie:12`.
    item TEXT NOT NULL,
    -- The origin's library it is in.
    library TEXT NOT NULL,
    -- The pair row that cached it. A server id is a label, not a key: a
    -- row kept unavailable is written again only by that pair, or by one
    -- with its key, never by another server that pairs under the same
    -- server id later (M2).
    peer TEXT NOT NULL,
    -- 'movie', 'show', 'season', 'episode', 'artist', 'album', 'track'.
    kind TEXT NOT NULL,
    -- The origin's ids of its season or album, and its show or artist.
    parent TEXT,
    grandparent TEXT,
    -- What it is listed and ordered by: its title, and its number in its
    -- parent (a season's, an episode's, a track's); and its year, which
    -- search goes by.
    title TEXT NOT NULL,
    position INTEGER,
    year INTEGER,
    -- The item as the origin sent it: federation/1's RemoteItem, as JSON.
    body TEXT NOT NULL,
    hash TEXT NOT NULL,
    -- 1 once the pair is gone but a user of this server still holds the
    -- item (watched it, liked it, has it in a playlist or downloaded it):
    -- kept so that is shown as no longer available, and neither browsed
    -- nor its artwork served.
    unavailable INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (origin, item)
);

CREATE INDEX federation_cache_items_listed
    ON federation_cache_items (origin, library, kind, title);
CREATE INDEX federation_cache_items_parent ON federation_cache_items (origin, parent);
CREATE INDEX federation_cache_items_grandparent
    ON federation_cache_items (origin, grandparent);

-- Each cached item's external ids: what progress per title (P8-03) and
-- merged search (P8-05) match on.
CREATE TABLE federation_cache_external_ids (
    origin TEXT NOT NULL,
    item TEXT NOT NULL,
    external_id TEXT NOT NULL,
    PRIMARY KEY (origin, item, external_id),
    FOREIGN KEY (origin, item)
        REFERENCES federation_cache_items (origin, item) ON DELETE CASCADE
);

CREATE INDEX federation_cache_external_ids_by_id
    ON federation_cache_external_ids (external_id);

-- Each image a cached item names: the only artwork served from the
-- origin, and only while one of its items is available.
CREATE TABLE federation_cache_item_artwork (
    origin TEXT NOT NULL,
    item TEXT NOT NULL,
    -- Its id on the origin.
    artwork TEXT NOT NULL,
    -- 'poster', 'background' or 'thumb'.
    kind TEXT NOT NULL,
    PRIMARY KEY (origin, artwork, item),
    FOREIGN KEY (origin, item)
        REFERENCES federation_cache_items (origin, item) ON DELETE CASCADE
);

CREATE INDEX federation_cache_item_artwork_by_item
    ON federation_cache_item_artwork (origin, item);

-- A sync's answer while it is read: its pages are kept here, and only a
-- whole answer within the limits is written to the cache, in one
-- transaction; one cut short, or past a limit, is dropped, and the cache
-- and the library's cursor stay as they were.
CREATE TABLE federation_cache_staged (
    origin TEXT NOT NULL,
    library TEXT NOT NULL,
    item TEXT NOT NULL,
    -- The item's hash and RemoteItem as JSON; both NULL for an item the
    -- answer names as gone.
    hash TEXT,
    body TEXT,
    PRIMARY KEY (origin, library, item)
);

-- Images fetched from an origin, as files under the state directory:
-- posters a few at a time after a sync, the rest when first asked for.
-- Each origin's take at most so many bytes, and all origins' together at
-- most so many more; the least recently used go first.
CREATE TABLE federation_cache_files (
    origin TEXT NOT NULL,
    artwork TEXT NOT NULL,
    -- The file's name in the origin's directory.
    file TEXT NOT NULL,
    content_type TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    -- Unix seconds.
    last_used INTEGER NOT NULL,
    PRIMARY KEY (origin, artwork)
);

CREATE INDEX federation_cache_files_lru ON federation_cache_files (origin, last_used);
CREATE INDEX federation_cache_files_lru_all ON federation_cache_files (last_used);

-- Each origin's files' bytes in all, kept by the triggers below as files
-- come and go: what the limits are held to, without summing every file
-- each time one is kept.
CREATE TABLE federation_cache_bytes (
    origin TEXT PRIMARY KEY NOT NULL,
    bytes INTEGER NOT NULL
);

CREATE TRIGGER federation_cache_file_kept AFTER INSERT ON federation_cache_files
BEGIN
    INSERT OR IGNORE INTO federation_cache_bytes (origin, bytes) VALUES (NEW.origin, 0);
    UPDATE federation_cache_bytes SET bytes = bytes + NEW.bytes WHERE origin = NEW.origin;
END;

CREATE TRIGGER federation_cache_file_replaced AFTER UPDATE OF bytes ON federation_cache_files
BEGIN
    UPDATE federation_cache_bytes SET bytes = bytes - OLD.bytes + NEW.bytes
    WHERE origin = NEW.origin;
END;

CREATE TRIGGER federation_cache_file_gone AFTER DELETE ON federation_cache_files
BEGIN
    UPDATE federation_cache_bytes SET bytes = bytes - OLD.bytes WHERE origin = OLD.origin;
END;
