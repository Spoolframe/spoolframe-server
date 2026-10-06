-- What signed an admin API session in (P3-18): 'web' for the web UI's
-- cookie sessions, 'cli' for the rest (the CLI, scripts). NULL for a
-- client session, whose device has the platform, and for an API session
-- from before this was kept.
ALTER TABLE sessions ADD COLUMN client TEXT;
