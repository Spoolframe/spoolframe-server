-- What this server's admin calls a pair (`federation_peerRename`): shown
-- to its users and admins in place of `display_name`, which stays what
-- the pair called itself when it was made. NULL for none.
ALTER TABLE federation_peers ADD COLUMN alias TEXT;
-- Whether the pair's own Home row ("New on …") is shown to this server's
-- users (`federation_peerHomeRow`): 0 leaves it out for every user. Its
-- libraries are listed, searched and in Recently added either way.
ALTER TABLE federation_peers ADD COLUMN home_row INTEGER NOT NULL DEFAULT 1;
