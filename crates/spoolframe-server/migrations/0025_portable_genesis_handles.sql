-- P7-08: a fresh install holding only a security key knows the handle its
-- credential carries (the genesis handle), not the identity's id, which is
-- a hash of the whole genesis. Looking a record up by handle needs it as a
-- column. 64 lowercase hex digits, as the genesis JSON writes it.
ALTER TABLE portable_geneses ADD COLUMN handle TEXT;
UPDATE portable_geneses SET handle = lower(json_extract(genesis, '$.handle'));
CREATE INDEX portable_geneses_by_handle ON portable_geneses (handle);
