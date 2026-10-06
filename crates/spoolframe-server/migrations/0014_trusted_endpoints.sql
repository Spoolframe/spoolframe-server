-- P4-01: trusted devices. A row: this device (its Iroh endpoint) may start
-- a session for this user without a full sign-in (`ResumeProfile`). No
-- row: it may not, the default. A PIN unlock never makes or renews one.
--
-- Rows end when the user signs out on the device, when their sessions
-- there are revoked by anyone, when the user's password is set or reset,
-- and when the user is disabled. One unused for `auth.trust_idle_days`
-- lapses: refused, and removed when it is next asked for.
CREATE TABLE trusted_endpoints (
    user_id TEXT NOT NULL REFERENCES users (id),
    endpoint_id TEXT NOT NULL REFERENCES devices (endpoint_id),
    -- The client opens this profile by itself at start. It turns the PIN
    -- requirement off for this user on this device, and the PIN
    -- requirement turns it off.
    auto_login INTEGER NOT NULL DEFAULT 0 CHECK (auto_login IN (0, 1)),
    created_at INTEGER NOT NULL,
    -- A resume, or a use of the user's session on the device, written at
    -- most once a minute as sessions are.
    last_used_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, endpoint_id)
);

-- At most one user opens by itself on a device.
CREATE UNIQUE INDEX trusted_endpoints_one_auto_login
    ON trusted_endpoints (endpoint_id) WHERE auto_login = 1;
