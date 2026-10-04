-- What kind of client each device runs ('linux', 'tvos', 'ios', 'macos',
-- 'cli'), as its Hello says, for device and session lists. NULL for a
-- device that has not connected since this was added.
ALTER TABLE devices ADD COLUMN platform TEXT;
