-- P8-04: the media path. A pair's plays served here are counted as a
-- play token is redeemed, not as it is minted, and of them those the
-- paired server redeemed and played on to the device (proxied); the rest
-- the device played itself (direct). Status and the Federation page show
-- both, so a pair whose plays all go through its server is seen, not
-- silent. An 'asserted' row's proxied counts the plays this server played
-- on to its own users' devices.
ALTER TABLE federation_plays ADD COLUMN proxied INTEGER NOT NULL DEFAULT 0;

-- The user's track preferences on the paired server, as JSON, as its
-- assertion gave them: the session the token opens chooses the audio and
-- subtitles by them, as the user's own server would. This server has
-- none of that user's own.
ALTER TABLE federation_tokens ADD COLUMN preferences TEXT;
