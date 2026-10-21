-- `Mobile` becomes HEVC 1080p 4 Mbit/s (the user's decision, 2026-10-21):
-- the phones it is for decode HEVC in hardware, and at the same rate it
-- looks better than H.264. `Mobile (H.264)` ships beside it, as `Mobile`
-- was, for the devices that play H.264 alone; downloads list only the
-- profiles a device plays.
--
-- Only a `Mobile` still as shipped changes: an admin's edit stays. Its
-- copies are H.264 1080p 4 Mbit/s, what `Mobile (H.264)` makes, so they
-- become that profile's rather than being made again; its requests stay
-- `Mobile`'s and ask for their items again, as an edit of its video does.
-- A server whose admin already has a `Mobile (H.264)` keeps theirs and
-- `Mobile` is left as it is.

CREATE TEMP TABLE mobile_as_shipped AS
SELECT 1 AS yes FROM optimized_profiles
WHERE name = 'Mobile' AND video_codec = 'h264' AND height = 1080 AND max_bitrate = 4000000
  AND NOT EXISTS (SELECT 1 FROM optimized_profiles WHERE name = 'Mobile (H.264)');

UPDATE optimized_copies SET target = 'profile:Mobile (H.264)'
WHERE target = 'profile:Mobile' AND EXISTS (SELECT 1 FROM mobile_as_shipped);

-- The request items' copies are H.264 ones now: asked for again, in HEVC.
UPDATE optimized_request_items SET copy_id = NULL, asked = 0, failure = NULL, round = round + 1
WHERE request_id IN (SELECT id FROM optimized_requests WHERE profile = 'Mobile')
  AND EXISTS (SELECT 1 FROM mobile_as_shipped);

-- Listed right after `Mobile`.
UPDATE optimized_profiles SET position = position + 1
WHERE position > (SELECT position FROM optimized_profiles WHERE name = 'Mobile')
  AND EXISTS (SELECT 1 FROM mobile_as_shipped);

INSERT INTO optimized_profiles (name, video_codec, height, max_bitrate, position, updated_at)
SELECT 'Mobile (H.264)', 'h264', 1080, 4000000, position + 1, 0
FROM optimized_profiles
WHERE name = 'Mobile' AND EXISTS (SELECT 1 FROM mobile_as_shipped);

UPDATE optimized_profiles SET video_codec = 'hevc'
WHERE name = 'Mobile' AND EXISTS (SELECT 1 FROM mobile_as_shipped);

DROP TABLE mobile_as_shipped;
