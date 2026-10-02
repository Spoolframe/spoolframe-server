-- P1-22: what a principal removed from Continue watching. Neither the
-- position nor the watched state changes; the next watch-state write for
-- the item clears it.
CREATE TABLE continue_hidden (
    principal TEXT NOT NULL,
    -- A movie's MediaId, or for an episode its show's: an episode hides
    -- its show, from Continue watching and Next up.
    media_id TEXT NOT NULL,
    hidden_at INTEGER NOT NULL,
    PRIMARY KEY (principal, media_id)
);

-- The catalog's stable ids for each hidden item, as watch_state_ids: when
-- the item turns up under a new MediaId, the hide moves to it.
CREATE TABLE continue_hidden_ids (
    principal TEXT NOT NULL,
    media_id TEXT NOT NULL,
    external_id TEXT NOT NULL,
    PRIMARY KEY (principal, external_id),
    FOREIGN KEY (principal, media_id) REFERENCES continue_hidden (principal, media_id)
        ON UPDATE CASCADE ON DELETE CASCADE
);
