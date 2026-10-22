-- A downloaded subtitle says whether it is a forced track (foreign parts
-- and signs only), as its search result did. Without it the track played
-- as the whole dialogue. Rows from before are 0: the site's result is
-- gone, and a full track is what they played as.
ALTER TABLE subtitle_downloads ADD COLUMN forced INTEGER NOT NULL DEFAULT 0;
