-- Each logged subtitle download says its site and its downloader's
-- display name then, so Settings › Subtitles shows a day's downloads per
-- site and per user. The log keeps removed downloads, which the site
-- counted all the same. Rows from before have neither ('') and are gone
-- a day later.
ALTER TABLE subtitle_download_log ADD COLUMN source TEXT NOT NULL DEFAULT '';
ALTER TABLE subtitle_download_log ADD COLUMN name TEXT NOT NULL DEFAULT '';
