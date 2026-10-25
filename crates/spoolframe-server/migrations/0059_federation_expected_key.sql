-- M8-06: the fingerprint of the new key a pair's admin said, out of
-- band, their server is moving to, as this server's admin entered it
-- (`federation_keyExpect`). A key with that fingerprint is held even when
-- the pair's held keys are full: four keys a stranger had held cannot
-- keep it out. The admin still confirms it. One per pair; NULL for none.
--
-- In the form the Federation page shows a key's fingerprint
-- (`K7QX-3MPA-9TRD-WF2H`). Kept from `expected_at` (Unix seconds) for as
-- long as a held key is, a week, and cleared when the pair's key is
-- confirmed, the key with it is rejected, or the pair is unpaired.
ALTER TABLE federation_peers ADD COLUMN expected_fingerprint TEXT;
ALTER TABLE federation_peers ADD COLUMN expected_at INTEGER;
-- The admin who entered it: an admin's username, as `paired_by` is.
ALTER TABLE federation_peers ADD COLUMN expected_by TEXT;
