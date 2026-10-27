-- A paired server's library sorted and filtered in its cache: what each
-- cached item is ordered and kept by, out of its body, so a page is one
-- indexed query rather than a read of every body.
--
-- A paired server from before these sends none of the new ones (sort
-- title, release, added date, ratings, file size, resolution, range):
-- they stay NULL until its next sync after it is upgraded, which sends
-- every item again, as each one's content hash changes with them. What
-- the body already has (its content rating, length, a film's release
-- and a show's genres) is copied out here, for the rows cached now.
ALTER TABLE federation_cache_items ADD COLUMN sort_title TEXT;
ALTER TABLE federation_cache_items ADD COLUMN released TEXT;
ALTER TABLE federation_cache_items ADD COLUMN added_at INTEGER;
-- Tenths: 0 to 100.
ALTER TABLE federation_cache_items ADD COLUMN rating INTEGER;
ALTER TABLE federation_cache_items ADD COLUMN audience_rating INTEGER;
ALTER TABLE federation_cache_items ADD COLUMN duration_ms INTEGER;
-- Bytes.
ALTER TABLE federation_cache_items ADD COLUMN size INTEGER;
-- '4k', '1080p', '720p' or 'sd'.
ALTER TABLE federation_cache_items ADD COLUMN resolution TEXT;
-- 'sdr', 'hdr' (not Dolby Vision) or 'dolby_vision'.
ALTER TABLE federation_cache_items ADD COLUMN dynamic_range TEXT;
ALTER TABLE federation_cache_items ADD COLUMN content_rating TEXT;

-- The sort title as spoolframe_types::browse::sort_title makes it: a
-- leading "The", "A" or "An" set aside, unless it is all there is. Every
-- row has one (a sync writes it too), so the title order and every
-- order's tiebreak walk an index.
UPDATE federation_cache_items
SET sort_title = CASE
        WHEN lower(substr(title, 1, 4)) = 'the ' AND ltrim(substr(title, 5)) <> ''
            THEN ltrim(substr(title, 5))
        WHEN lower(substr(title, 1, 2)) = 'a ' AND ltrim(substr(title, 3)) <> ''
            THEN ltrim(substr(title, 3))
        WHEN lower(substr(title, 1, 3)) = 'an ' AND ltrim(substr(title, 4)) <> ''
            THEN ltrim(substr(title, 4))
        ELSE title
    END;

UPDATE federation_cache_items
SET duration_ms = json_extract(body, '$.duration_ms'),
    content_rating = json_extract(body, '$.content_rating'),
    released = CASE kind WHEN 'movie' THEN json_extract(body, '$.date') END
WHERE json_valid(body);

-- Each available cached item's genres, as the origin named them, with
-- the item's library and kind copied, so a library's genres are counted
-- and filtered by out of the index alone. An item kept unavailable has
-- none: it is in no library that is browsed.
CREATE TABLE federation_cache_genres (
    origin TEXT NOT NULL,
    item TEXT NOT NULL,
    genre TEXT NOT NULL,
    library TEXT NOT NULL,
    kind TEXT NOT NULL,
    PRIMARY KEY (origin, item, genre),
    FOREIGN KEY (origin, item)
        REFERENCES federation_cache_items (origin, item) ON DELETE CASCADE
);

-- Which of a library's items have a genre, any case.
CREATE INDEX federation_cache_genres_by_genre
    ON federation_cache_genres (origin, library, kind, genre COLLATE NOCASE, item);

INSERT OR IGNORE INTO federation_cache_genres (origin, item, genre, library, kind)
SELECT i.origin, i.item, g.value, i.library, i.kind
FROM federation_cache_items i, json_each(i.body, '$.genres') g
WHERE json_valid(i.body) AND g.type = 'text' AND i.unavailable = 0;

-- What a library's items are counted by, for the values to filter by.
CREATE INDEX federation_cache_items_by_content_rating
    ON federation_cache_items (origin, library, kind, unavailable, content_rating);
CREATE INDEX federation_cache_items_by_resolution
    ON federation_cache_items (origin, library, kind, unavailable, resolution);
CREATE INDEX federation_cache_items_by_dynamic_range
    ON federation_cache_items (origin, library, kind, unavailable, dynamic_range);

-- One per sort key, for a library's page in that order: the library's
-- available items of its top kind, by the key, then the tiebreak (sort
-- title, item), so a page is a walk of the index, not a sort of the
-- library, and a count reads only the index.
CREATE INDEX federation_cache_items_by_sort_title
    ON federation_cache_items
    (origin, library, kind, unavailable, sort_title COLLATE NOCASE, item);
CREATE INDEX federation_cache_items_by_released
    ON federation_cache_items
    (origin, library, kind, unavailable, released, sort_title COLLATE NOCASE, item);
CREATE INDEX federation_cache_items_by_added
    ON federation_cache_items
    (origin, library, kind, unavailable, added_at, sort_title COLLATE NOCASE, item);
CREATE INDEX federation_cache_items_by_rating
    ON federation_cache_items
    (origin, library, kind, unavailable, rating, sort_title COLLATE NOCASE, item);
CREATE INDEX federation_cache_items_by_audience_rating
    ON federation_cache_items
    (origin, library, kind, unavailable, audience_rating, sort_title COLLATE NOCASE, item);
CREATE INDEX federation_cache_items_by_duration
    ON federation_cache_items
    (origin, library, kind, unavailable, duration_ms, sort_title COLLATE NOCASE, item);
CREATE INDEX federation_cache_items_by_size
    ON federation_cache_items
    (origin, library, kind, unavailable, size, sort_title COLLATE NOCASE, item);
CREATE INDEX federation_cache_items_by_year
    ON federation_cache_items (origin, library, kind, unavailable, year, item);

-- On the server that shares: what an item's own record said beyond its
-- external ids (every genre, its dynamic range), read with them, as JSON.
-- NULL for a row from before: read again at the next walk, once.
ALTER TABLE federation_external_ids ADD COLUMN details TEXT;
