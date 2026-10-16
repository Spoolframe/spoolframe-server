-- PX-53: a playlist's uploaded cover, as the ArtworkId clients fetch it
-- by ('image:playlist/<hash>'); its file is state_dir/images/playlists/
-- <hash>.jpg. NULL: the playlist shows its mosaic, worked out when read
-- and never kept here. Several playlists may name the same file; it goes
-- when the last of them stops naming it.
ALTER TABLE playlists ADD COLUMN art TEXT;

CREATE INDEX playlists_art ON playlists (art) WHERE art IS NOT NULL;
