-- P8-06, G8-02: a status answer that raced a newer preparation of the same
-- download kept the older state over the newer. Every write of a row bumps
-- its revision, and an answer is kept only for the revision it was asked
-- under.
ALTER TABLE federation_downloads ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
