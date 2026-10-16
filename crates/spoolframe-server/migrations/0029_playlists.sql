-- PX-40: playlists. A name, an owner, the kinds of item it may hold and
-- an ordered list of entries; shared with chosen users, or seen by every
-- user (server-wide). The rules are spoolframe-policy's `playlists`.

CREATE TABLE playlists (
    -- A UUIDv7, as text.
    id TEXT PRIMARY KEY NOT NULL,
    -- A Principal ('local:<uuid>', 'portable:<hex>'): who made it.
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    -- The kinds it may hold, as bits: 1 movies, 2 episodes, 4 tracks.
    -- Never 0, and only ever widened.
    kinds INTEGER NOT NULL,
    -- Seen by every user, edited by admins (made by an admin).
    server_wide INTEGER NOT NULL DEFAULT 0,
    -- Raised by every change, its entries' or its own: what an editor's
    -- `EditPlaylist` names, and what events carry.
    revision INTEGER NOT NULL DEFAULT 0,
    -- The oldest revision whose entry changes `playlist_edits` still
    -- holds: an edit made on an older one cannot be redone.
    recorded_from INTEGER NOT NULL DEFAULT 0,
    -- Unix seconds.
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX playlists_by_owner ON playlists (owner);
CREATE INDEX playlists_server_wide ON playlists (server_wide) WHERE server_wide = 1;

-- Who a playlist is shared with, each read-only or editable.
CREATE TABLE playlist_shares (
    playlist_id TEXT NOT NULL REFERENCES playlists (id) ON DELETE CASCADE,
    principal TEXT NOT NULL,
    can_edit INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, principal)
);

CREATE INDEX playlist_shares_by_principal ON playlist_shares (principal);

-- A playlist's entries: one place each, so an item may be in it twice.
-- `entry_id` is AUTOINCREMENT so an id is never handed out again: an
-- editor's edit names entries, and one removed must not come back as
-- another.
CREATE TABLE playlist_entries (
    entry_id INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id TEXT NOT NULL REFERENCES playlists (id) ON DELETE CASCADE,
    -- 0, 1, 2… in the playlist's order.
    position INTEGER NOT NULL,
    -- The item's MediaId, e.g. 'plex:episode:208620'.
    media_id TEXT NOT NULL,
    -- 'movie', 'episode' or 'track'.
    kind TEXT NOT NULL,
    -- An episode's show, a track's album and artist: what an item screen
    -- asks "which playlists hold any of these" by.
    show_id TEXT,
    album_id TEXT,
    artist_id TEXT,
    added_at INTEGER NOT NULL
);

CREATE INDEX playlist_entries_order ON playlist_entries (playlist_id, position);
CREATE INDEX playlist_entries_media ON playlist_entries (media_id);
CREATE INDEX playlist_entries_show ON playlist_entries (show_id) WHERE show_id IS NOT NULL;
CREATE INDEX playlist_entries_album ON playlist_entries (album_id) WHERE album_id IS NOT NULL;
CREATE INDEX playlist_entries_artist ON playlist_entries (artist_id) WHERE artist_id IS NOT NULL;

-- Each entry's item's stable ids (as watch_state_ids): when Plex rebuilds
-- a library and the item has a new key, the entry follows it.
CREATE TABLE playlist_entry_ids (
    entry_id INTEGER NOT NULL REFERENCES playlist_entries (entry_id) ON DELETE CASCADE,
    external_id TEXT NOT NULL,
    PRIMARY KEY (entry_id, external_id)
);

-- The entries each revision removed or moved: an edit made on an older
-- revision is redone unless it touches one of them. Not a reference: the
-- entry may be gone.
CREATE TABLE playlist_edits (
    playlist_id TEXT NOT NULL REFERENCES playlists (id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    entry_id INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, revision, entry_id)
);

-- Each user's place in each playlist, and how they play it. `entry_id`
-- NULL: from the top. The settings outlive the place.
CREATE TABLE playlist_places (
    principal TEXT NOT NULL,
    playlist_id TEXT NOT NULL REFERENCES playlists (id) ON DELETE CASCADE,
    -- The entry playing or last played; not a reference, as it may go.
    entry_id INTEGER,
    shuffle INTEGER NOT NULL DEFAULT 0,
    -- 'off', 'one' or 'all'.
    repeat TEXT NOT NULL DEFAULT 'off',
    skip_watched INTEGER NOT NULL DEFAULT 0,
    -- A shuffled round: the entries in the order they play, as a JSON
    -- array; NULL when not shuffling.
    round TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (principal, playlist_id)
);

-- The playlists each user pinned to their sidebar, in their order.
CREATE TABLE playlist_pins (
    principal TEXT NOT NULL,
    playlist_id TEXT NOT NULL REFERENCES playlists (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    PRIMARY KEY (principal, playlist_id)
);

-- A pinned playlist that left a user's Pinned (unshared from them, or
-- deleted), until a client of theirs shows the note and dismisses it.
-- Not a reference: the playlist may be gone.
CREATE TABLE playlist_pin_notes (
    principal TEXT NOT NULL,
    playlist_id TEXT NOT NULL,
    -- Its name then.
    name TEXT NOT NULL,
    -- 'unshared' or 'deleted'.
    reason TEXT NOT NULL,
    noted_at INTEGER NOT NULL,
    PRIMARY KEY (principal, playlist_id)
);
