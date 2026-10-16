-- PX-43: the built-in music playlists, Liked and All. Each user has one
-- of each, made the first time it is needed, owned by them, personal and
-- never shared. Liked keeps its entries as any playlist does; All keeps
-- none, as it is every track the user may see, worked out at each read:
-- its row is here for its place and play settings (playlist_places) and
-- its pin. The rules are spoolframe-policy's `builtin_*`.

-- 'liked' or 'all'; NULL for a playlist a user made.
ALTER TABLE playlists ADD COLUMN builtin TEXT;

CREATE UNIQUE INDEX playlists_builtin ON playlists (owner, builtin) WHERE builtin IS NOT NULL;
