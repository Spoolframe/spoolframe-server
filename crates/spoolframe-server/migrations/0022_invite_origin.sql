-- PX-44: invites are made in the apps too, so each says where it was
-- made, for the admins' list ("by Andrey on iPhone").

-- 'web' (the web UI's admin pages), 'cli' (`spoolframe-server invite
-- create`) or 'device' (an app, on the device below). Before this only
-- the web UI and the CLI made invites, and the CLI's are the ones its
-- name marks as made by.
ALTER TABLE invites ADD COLUMN origin TEXT NOT NULL DEFAULT 'web'
    CHECK (origin IN ('web', 'cli', 'device'));
UPDATE invites SET origin = 'cli' WHERE created_by = 'server CLI';

-- For an invite made in an app: the device's hex EndpointId, the name
-- and platform its Hello gave. NULL for the others.
ALTER TABLE invites ADD COLUMN origin_endpoint_id TEXT;
ALTER TABLE invites ADD COLUMN origin_device_name TEXT;
ALTER TABLE invites ADD COLUMN origin_platform TEXT;
