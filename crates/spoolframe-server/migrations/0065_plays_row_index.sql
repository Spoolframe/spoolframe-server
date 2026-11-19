-- Home's "Recently played" row (`RecentlyPlayedRow`) reads one user's
-- counted plays of the kinds it shows, every column a card is made of:
-- this covers it, so the row is read from the index alone, a user's
-- plays and no one else's. Only counted plays are in it.
CREATE INDEX plays_row ON plays (
    principal, kind, media_id, show_id, season_id, album_id, source_kind, source_id, ended_at
) WHERE counted = 1;
