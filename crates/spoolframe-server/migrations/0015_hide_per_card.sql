-- P1-22: "Remove" removes the card it is on, a movie or an episode, and no
-- longer a whole show. continue_hidden.media_id is now always the card's
-- own MediaId. A show's id here is a hide made before, when removing an
-- episode hid its show: it is dropped, and the show comes back to Home.
-- The second segment of a MediaId is its kind (`plex:show:10`).
DELETE FROM continue_hidden_ids
WHERE substr(media_id, instr(media_id, ':') + 1, 5) = 'show:';

DELETE FROM continue_hidden
WHERE substr(media_id, instr(media_id, ':') + 1, 5) = 'show:';
