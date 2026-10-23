-- P8-03: delegated authentication between paired servers.

-- On the server whose item is played (SA): the tokens it minted for a
-- paired server's users, each for one item, from one device, for
-- minutes. Only the token's hash is kept: the bearer is given once, over
-- the pair's connection. P8-04 redeems them; a revoked grant and
-- unpairing delete the pair's, and expired ones are pruned.
CREATE TABLE federation_tokens (
    -- blake3 of the token, hex.
    token_hash TEXT PRIMARY KEY NOT NULL,
    -- The pair whose user it is for.
    peer TEXT NOT NULL REFERENCES federation_peers (id),
    -- `remote:<serverid>:<uuid>`: the pair's user, by their stable id there.
    principal TEXT NOT NULL,
    -- The item, as this server knows it (`plex:movie:12`).
    item TEXT NOT NULL,
    -- The shared library it is in: a revoked grant deletes its tokens.
    library TEXT NOT NULL,
    -- The device's endpoint id, hex: the only one it works from.
    device TEXT NOT NULL,
    -- 'play'; P8-07 adds 'download'.
    scope TEXT NOT NULL,
    -- Unix seconds.
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    -- When P8-04 redeemed it; a token is used once.
    used_at INTEGER
);

CREATE INDEX federation_tokens_grant ON federation_tokens (peer, library);
CREATE INDEX federation_tokens_expiry ON federation_tokens (expires_at);

-- Plays across a pair, one number per server: never per user. 'served'
-- rows are on the server whose item is played, counted as it mints a
-- token; 'asserted' rows on the server whose user plays, counted as it
-- vouches for them. `refused` counts the assertions refused, on each side.
CREATE TABLE federation_plays (
    peer TEXT NOT NULL REFERENCES federation_peers (id),
    side TEXT NOT NULL,
    plays INTEGER NOT NULL DEFAULT 0,
    refused INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (peer, side)
);
