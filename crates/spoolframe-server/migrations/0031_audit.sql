-- PX-42: the audit list the admin pages show. What an admin did through
-- an MCP client, and which apps were connected and disconnected; kept
-- generic, so the web UI's own admin changes can be recorded here later.

CREATE TABLE audit_events (
    -- AUTOINCREMENT: newest first is by id, and an id is never reused.
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    -- Unix seconds.
    at INTEGER NOT NULL,
    -- Who: a user's id (a UUID), as the users table has it. Not a
    -- reference: the record outlives what it names.
    actor TEXT NOT NULL,
    -- Through what: an MCP client's registered name, or 'web'.
    client TEXT NOT NULL,
    -- What, e.g. 'disable_user', 'app.connect', 'app.revoke'.
    action TEXT NOT NULL,
    -- To what: a username, a session's or a playback's id, an app's name.
    target TEXT,
    -- 'ok', or why not.
    outcome TEXT NOT NULL
);
