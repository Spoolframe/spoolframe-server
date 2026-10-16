-- PX-42: the server as its own OAuth 2.1 authorization server, for MCP
-- clients. A client registers itself (RFC 7591), a user consents to some
-- of the scopes it asks for, and it gets a code, then tokens. Codes and
-- tokens are kept by their hash; none is stored as it was handed out.

-- Clients that registered themselves. Anyone may register, so a client
-- no user granted anything is deleted after a day.
CREATE TABLE oauth_clients (
    -- The client_id handed out: random, hex.
    id TEXT PRIMARY KEY NOT NULL,
    -- The name it registered (client_name): the app's own word for
    -- itself, never checked, so consent marks it as the app's.
    name TEXT NOT NULL,
    -- Its redirect URIs, as a JSON array of strings.
    redirect_uris TEXT NOT NULL,
    -- Unix seconds.
    created_at INTEGER NOT NULL
);

-- What a user consented to for a client: one grant per client and user,
-- replaced by the next consent. Revoking deletes it, and its codes and
-- tokens go with it.
CREATE TABLE oauth_grants (
    -- A UUIDv7, as text.
    id TEXT PRIMARY KEY NOT NULL,
    client_id TEXT NOT NULL REFERENCES oauth_clients (id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users (id),
    -- The scopes granted, space-separated, as OAuth writes them.
    scopes TEXT NOT NULL,
    -- The redirect URI consent sent the code to: its host is what My
    -- profile shows beside the client's name.
    redirect_uri TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    -- The last MCP call made with its tokens, Unix seconds.
    last_call_at INTEGER,
    -- The UTC day (Unix seconds / 86400) `calls_today` counts; an older
    -- day reads as no calls.
    calls_day INTEGER,
    calls_today INTEGER NOT NULL DEFAULT 0,
    UNIQUE (client_id, user_id)
);

CREATE INDEX oauth_grants_by_user ON oauth_grants (user_id);

-- Authorization codes: one use, a minute's life. A used one is kept until
-- it expires, so a second use is seen (and ends its grant).
CREATE TABLE oauth_codes (
    -- BLAKE3 of the code, hex.
    code_hash TEXT PRIMARY KEY NOT NULL,
    grant_id TEXT NOT NULL REFERENCES oauth_grants (id) ON DELETE CASCADE,
    -- The redirect URI the authorization request named: the token request
    -- must name the same.
    redirect_uri TEXT NOT NULL,
    -- PKCE: the S256 challenge, base64url without padding.
    code_challenge TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER
);

-- Access and refresh tokens. A refresh token is used once: using it
-- marks it (`used_at`) and hands out a new pair; a second use is a token
-- that leaked, and ends the grant.
CREATE TABLE oauth_tokens (
    -- BLAKE3 of the token, hex.
    token_hash TEXT PRIMARY KEY NOT NULL,
    grant_id TEXT NOT NULL REFERENCES oauth_grants (id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('access', 'refresh')),
    expires_at INTEGER NOT NULL,
    used_at INTEGER
);

CREATE INDEX oauth_tokens_by_grant ON oauth_tokens (grant_id);
