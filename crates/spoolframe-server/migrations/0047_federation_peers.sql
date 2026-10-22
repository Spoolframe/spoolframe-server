-- P8-01: servers paired with this one. Both sides of a pair keep a row:
-- the server whose `federation` invite was redeemed, and the one that
-- redeemed it. A server's identity is its endpoint key; its server id is
-- a label that outlives a rotated key, so a row pins the two together.
--
-- Unpairing sets `revoked_at` and keeps the row: later tables (grants,
-- the cached catalog, P8-02 and P8-03) reference it. Pairing again makes
-- a new row.
CREATE TABLE federation_peers (
    -- A UUIDv7, as text.
    id TEXT PRIMARY KEY NOT NULL,
    -- The peer's ServerId: 16 characters of base32.
    server_id TEXT NOT NULL,
    -- The peer's EndpointId, hex: the key its connections prove.
    endpoint_id TEXT NOT NULL,
    -- What the peer called itself when the pair was made.
    display_name TEXT NOT NULL,
    -- Where it may be dialled again: JSON arrays of `ip:port` and of
    -- relay URLs, as an invite link carries them. Hints.
    addrs TEXT NOT NULL DEFAULT '[]',
    relays TEXT NOT NULL DEFAULT '[]',
    -- Who paired it here: the username of the admin who made the invite
    -- (or 'server CLI'), on the side whose invite it was; the admin who
    -- redeemed it, on the other.
    paired_by TEXT NOT NULL,
    -- The invite redeemed, on the side that made it; NULL on the side
    -- that redeemed another server's.
    invite_id TEXT REFERENCES invites (id),
    -- Unix seconds.
    paired_at INTEGER NOT NULL,
    revoked_at INTEGER
);

-- One live pair per key and per server id.
CREATE UNIQUE INDEX federation_peers_live_endpoint
    ON federation_peers (endpoint_id) WHERE revoked_at IS NULL;
CREATE UNIQUE INDEX federation_peers_live_server
    ON federation_peers (server_id) WHERE revoked_at IS NULL;

-- New keys that said they are a live pair's server, held until an admin
-- confirms one (the pin moves to it, and the pair's keys are cleared) or
-- rejects it. A few per pair, never replaced: a key that says it is the
-- pair cannot push out the one the admin is looking at. A rejected key
-- keeps its row, refused, until it expires; a held key expires when it
-- has not been seen for a while.
CREATE TABLE federation_peer_held_keys (
    peer TEXT NOT NULL REFERENCES federation_peers (id),
    -- Its EndpointId, hex.
    endpoint_id TEXT NOT NULL,
    -- Unix seconds.
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    rejected_at INTEGER,
    PRIMARY KEY (peer, endpoint_id)
);
