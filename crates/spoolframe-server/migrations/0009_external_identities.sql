-- P3-04: identities at an OpenID Connect provider, each linked to a local
-- user, who may then sign in with it. Never matched by email: a user links
-- one from My profile while signed in, and only then does it sign anyone
-- in (§24A). One identity is one user's; a user has at most one per
-- provider (issuer).
CREATE TABLE external_identities (
    user_id INTEGER NOT NULL REFERENCES users (id),
    -- The provider's issuer and the identity's `sub` there: together what
    -- the identity is.
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    -- What to recognise it by (its email or username at the provider, as
    -- the ID token said when it was linked); never used to match.
    display TEXT,
    linked_at INTEGER NOT NULL,
    last_used_at INTEGER,
    PRIMARY KEY (issuer, subject),
    UNIQUE (user_id, issuer)
);
