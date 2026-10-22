-- About how many bytes a download's file will be, as `PrepareDownload`
-- said, kept so `DownloadStatus` can say it too: a device that only asks
-- for status shows the size before the file is ready. NULL where the
-- server could not tell, and for a row from before this.
ALTER TABLE downloads ADD COLUMN expected_size INTEGER;
