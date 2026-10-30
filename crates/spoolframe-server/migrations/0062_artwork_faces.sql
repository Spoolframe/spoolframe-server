-- The faces in a backdrop (PX-64, experimental `smart-crop`): one row per
-- artwork scanned, `faces` a JSON array of spoolframe_protocol's
-- ArtworkFace, `[]` for scanned with none found. No row: not scanned yet.
-- Artwork ids change with the image, so a row never goes stale.
CREATE TABLE artwork_faces (
    artwork_id TEXT PRIMARY KEY NOT NULL,
    faces TEXT NOT NULL,
    scanned_at INTEGER NOT NULL
) WITHOUT ROWID;
