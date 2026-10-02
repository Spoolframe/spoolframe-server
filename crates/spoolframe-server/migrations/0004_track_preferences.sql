-- P1-20: how each principal wants tracks chosen, and what they picked for
-- a series or a movie. Both are small documents the server reads whole,
-- so they are stored as JSON; their shape is spoolframe-types'
-- TrackPreferences and spoolframe-server's TrackOverride, whose serde
-- defaults let old rows read after fields are added.

CREATE TABLE track_preferences (
    principal TEXT PRIMARY KEY,
    preferences TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE track_overrides (
    principal TEXT NOT NULL,
    -- A show's MediaId for an episode, else the item's own.
    scope TEXT NOT NULL,
    override TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (principal, scope)
);
