-- Why a download failed, beside `failure`'s words, for the device to
-- choose what to show: `server_stopped_converting` (its conversion
-- failed, or the server stopped while it was being prepared),
-- `source_missing` (its media file is gone or changed) or `other`. A row
-- that failed before this is NULL, read as `other`.
ALTER TABLE downloads ADD COLUMN failure_cause TEXT;
