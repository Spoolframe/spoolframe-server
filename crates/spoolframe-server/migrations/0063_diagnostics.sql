-- Diagnostics an app sent for the admins to read (`SendDiagnostics`,
-- feature `spoolframe:diagnostics`). Who and where are copied as they were
-- when it arrived, so a report outlives its session, device and user.
-- Kept: the newest 20 of each device, none older than 30 days.
CREATE TABLE diagnostics_reports (
    -- A UUIDv7.
    id TEXT PRIMARY KEY NOT NULL,
    -- A Principal (`local:…`, `portable:…`), and what it was called then.
    principal TEXT NOT NULL,
    user_name TEXT NOT NULL,
    -- The device's hex EndpointId, and its name from its Hello, if any.
    endpoint_id TEXT NOT NULL,
    device_name TEXT,
    -- The hash sessions.token_hash keys the session by: one report a
    -- minute from a session. Never the token itself.
    session_hash TEXT NOT NULL,
    -- As the app sent them.
    device_model TEXT NOT NULL,
    os_name TEXT NOT NULL,
    os_version TEXT NOT NULL,
    app_name TEXT NOT NULL,
    app_version TEXT NOT NULL,
    note TEXT,
    log TEXT NOT NULL,
    -- Unix seconds.
    received_at INTEGER NOT NULL
);

CREATE INDEX diagnostics_reports_by_time ON diagnostics_reports (received_at);
CREATE INDEX diagnostics_reports_by_device ON diagnostics_reports (endpoint_id, received_at);
CREATE INDEX diagnostics_reports_by_session ON diagnostics_reports (session_hash, received_at);
