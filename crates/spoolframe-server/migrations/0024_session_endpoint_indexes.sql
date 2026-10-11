-- Review 2026-10-11 M18: whether a device is signed in is asked on every
-- relay admission and every minute for each known device, by endpoint.
-- Without these it read every session row; dead rows are now purged too
-- (Store::purge_dead_sessions), so the tables stop growing for good.
CREATE INDEX sessions_by_endpoint ON sessions (endpoint_id) WHERE kind = 'endpoint';
CREATE INDEX portable_sessions_by_endpoint ON portable_sessions (endpoint_id);
