-- P8-02, on the server that shares: what it last gave each pair of each
-- library shared with it, so a sync answers with the changes since. Plex
-- has no change feed, so the server walks the library when the pair
-- asks, and diffs what it finds against this.
--
-- A state is named by its cursor. At most two are kept per pair and
-- library: the one the pair said it has (`since`), and the one last sent,
-- which the pair has only once it names it. A sync that names neither
-- gets the whole library.
CREATE TABLE federation_sync_states (
    peer TEXT NOT NULL REFERENCES federation_peers (id),
    library TEXT NOT NULL,
    cursor TEXT NOT NULL,
    -- Unix seconds.
    made_at INTEGER NOT NULL,
    PRIMARY KEY (peer, library, cursor)
);

-- Each item a state holds, by its id on this server, and its content
-- hash as sent.
CREATE TABLE federation_sync_items (
    peer TEXT NOT NULL,
    library TEXT NOT NULL,
    cursor TEXT NOT NULL,
    item TEXT NOT NULL,
    hash TEXT NOT NULL,
    PRIMARY KEY (peer, library, cursor, item),
    FOREIGN KEY (peer, library, cursor)
        REFERENCES federation_sync_states (peer, library, cursor) ON DELETE CASCADE
);

-- Each image a state's items name: the only artwork the pair may fetch.
CREATE TABLE federation_sync_artwork (
    peer TEXT NOT NULL,
    library TEXT NOT NULL,
    cursor TEXT NOT NULL,
    artwork TEXT NOT NULL,
    PRIMARY KEY (peer, artwork, library, cursor),
    FOREIGN KEY (peer, library, cursor)
        REFERENCES federation_sync_states (peer, library, cursor) ON DELETE CASCADE
);

-- A state's images by the state: what its deletion cascades through.
CREATE INDEX federation_sync_artwork_by_state
    ON federation_sync_artwork (peer, library, cursor);

-- An item's external ids, as last read from the catalog, with the hash
-- of the rest of it then: a listing gives no external ids, and asking
-- the catalog for each item at every sync would cost a request per item.
-- They are read again when the rest of the item changes.
CREATE TABLE federation_external_ids (
    item TEXT PRIMARY KEY NOT NULL,
    -- The library walked when they were read: a walk reads its own.
    library TEXT NOT NULL,
    shallow TEXT NOT NULL,
    -- A JSON array of strings.
    external_ids TEXT NOT NULL
);
CREATE INDEX federation_external_ids_by_library
    ON federation_external_ids (library);
