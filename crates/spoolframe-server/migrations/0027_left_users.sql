-- A local user who left the server from an app (PX-47): set aside, not
-- deleted. The account is also disabled (`disabled_at`), so it cannot sign
-- in; `left_at` tells the admin why. Enabling it again clears both.
ALTER TABLE users ADD COLUMN left_at INTEGER;
