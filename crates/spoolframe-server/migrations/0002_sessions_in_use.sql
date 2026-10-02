-- P1-12: names for people, and sessions that stay alive while used.

-- What clients show for a user ("Who's watching"); the username when NULL.
ALTER TABLE users ADD COLUMN display_name TEXT;

-- When the session was last used. Endpoint sessions expire a fixed time
-- after it and are renewed on use, so a device in use never signs out.
ALTER TABLE sessions ADD COLUMN last_used_at INTEGER;
UPDATE sessions SET last_used_at = created_at;
