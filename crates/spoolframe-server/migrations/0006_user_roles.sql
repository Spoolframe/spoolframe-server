-- P2-09: a named role in place of the is_admin flag, so a role can be
-- shown, switched on, and added to without another flag.
ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'consumer'
    CHECK (role IN ('admin', 'consumer'));
UPDATE users SET role = CASE WHEN is_admin THEN 'admin' ELSE 'consumer' END;
ALTER TABLE users DROP COLUMN is_admin;
