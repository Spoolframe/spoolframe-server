//! Saving a file's bytes as they come, in blocks at offsets: what a
//! download's transfer feeds (8 MiB reads, in order), and what a writer
//! fed from playback reads would (PX-51's tracks kept from listening:
//! blocks in any order, as the player seeks). It knows which ranges it
//! has, is complete once it has them all, and checks the whole file's
//! SHA-256 when one is known before the file takes its final name.

use std::path::{Path, PathBuf};

use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// A file being written, `size` bytes in all.
#[derive(Debug)]
pub(crate) struct BlockWriter {
    path: PathBuf,
    file: tokio::fs::File,
    size: u64,
    /// The ranges written, `[start, end)`, sorted, apart and not touching.
    have: Vec<(u64, u64)>,
    /// Bytes written since the last [`BlockWriter::sync`].
    unsynced: u64,
}

/// Why [`BlockWriter::finish`] kept no file.
#[derive(Debug)]
pub(crate) enum Unfinished {
    /// Not every byte is there yet.
    Incomplete,
    /// Every byte is there, but they are not the file: another SHA-256.
    /// The part is removed.
    Damaged,
    Io(std::io::Error),
}

impl From<std::io::Error> for Unfinished {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl BlockWriter {
    /// Write the file of `size` bytes at `path`, keeping the first `kept`
    /// bytes already there: those known to be on disk (synced), or the
    /// file's length if that is less (what came after the last sync may
    /// be lost). The file is cut there.
    pub async fn open(path: &Path, size: u64, kept: u64) -> std::io::Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .await?;
        let kept = kept.min(file.metadata().await?.len()).min(size);
        file.set_len(kept).await?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            size,
            have: if kept > 0 {
                vec![(0, kept)]
            } else {
                Vec::new()
            },
            unsynced: 0,
        })
    }

    /// Write `bytes` at `offset`. A block past the end is refused; one
    /// over bytes already there writes them again.
    pub async fn write_at(&mut self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        let end = offset.saturating_add(bytes.len() as u64);
        if end > self.size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{end} is past the file's {} bytes", self.size),
            ));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        self.file.seek(std::io::SeekFrom::Start(offset)).await?;
        self.file.write_all(bytes).await?;
        self.unsynced += bytes.len() as u64;
        self.add(offset, end);
        Ok(())
    }

    fn add(&mut self, start: u64, end: u64) {
        let (mut start, mut end) = (start, end);
        self.have.retain(|&(s, e)| {
            if e < start || s > end {
                true
            } else {
                start = start.min(s);
                end = end.max(e);
                false
            }
        });
        let at = self.have.partition_point(|&(s, _)| s < start);
        self.have.insert(at, (start, end));
    }

    /// How many bytes from the start are all there: where an in-order
    /// writer goes on.
    pub fn contiguous(&self) -> u64 {
        match self.have.first() {
            Some(&(0, end)) => end,
            _ => 0,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.contiguous() == self.size
    }

    pub fn unsynced(&self) -> u64 {
        self.unsynced
    }

    /// Put what was written on disk: after this, [`BlockWriter::contiguous`]
    /// is safe to save as what a reopen keeps.
    pub async fn sync(&mut self) -> std::io::Result<()> {
        self.file.sync_data().await?;
        self.unsynced = 0;
        Ok(())
    }

    /// The whole file, renamed to `dest` once its SHA-256 is `sha256`
    /// (when one is known).
    pub async fn finish(mut self, dest: &Path, sha256: Option<&str>) -> Result<(), Unfinished> {
        if !self.is_complete() {
            return Err(Unfinished::Incomplete);
        }
        self.sync().await?;
        drop(self.file);
        if let Some(expected) = sha256 {
            let actual = super::store::sha256_file(&self.path).await?;
            if actual != expected {
                let _ = tokio::fs::remove_file(&self.path).await;
                return Err(Unfinished::Damaged);
            }
        }
        tokio::fs::rename(&self.path, dest).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sf-sdk-writer-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// As a player seeks: the end first, then the start, then the middle.
    #[tokio::test]
    async fn blocks_in_any_order_make_the_whole_file() {
        let dir = dir("order");
        let film: Vec<u8> = (0..100u8).collect();
        let mut writer = BlockWriter::open(&dir.join("part"), 100, 0).await.unwrap();
        writer.write_at(70, &film[70..]).await.unwrap();
        assert_eq!(writer.contiguous(), 0);
        writer.write_at(0, &film[..30]).await.unwrap();
        assert_eq!(writer.contiguous(), 30);
        assert!(!writer.is_complete());
        writer.write_at(25, &film[25..75]).await.unwrap();
        assert!(writer.is_complete());
        writer
            .finish(&dir.join("done"), Some(&sha(&film)))
            .await
            .unwrap();
        assert_eq!(std::fs::read(dir.join("done")).unwrap(), film);
        assert!(!dir.join("part").exists());
    }

    #[tokio::test]
    async fn a_file_with_a_gap_does_not_finish() {
        let dir = dir("gap");
        let mut writer = BlockWriter::open(&dir.join("part"), 10, 0).await.unwrap();
        writer.write_at(0, &[1; 4]).await.unwrap();
        writer.write_at(6, &[1; 4]).await.unwrap();
        assert!(matches!(
            writer.finish(&dir.join("done"), None).await,
            Err(Unfinished::Incomplete)
        ));
        assert!(!dir.join("done").exists());
    }

    #[tokio::test]
    async fn a_whole_file_of_other_bytes_is_damaged_and_removed() {
        let dir = dir("damaged");
        let mut writer = BlockWriter::open(&dir.join("part"), 3, 0).await.unwrap();
        writer.write_at(0, b"abd").await.unwrap();
        assert!(matches!(
            writer.finish(&dir.join("done"), Some(&sha(b"abc"))).await,
            Err(Unfinished::Damaged)
        ));
        assert!(!dir.join("part").exists() && !dir.join("done").exists());
    }

    #[tokio::test]
    async fn with_no_hash_known_a_whole_file_is_kept_as_it_is() {
        let dir = dir("no-hash");
        let mut writer = BlockWriter::open(&dir.join("part"), 3, 0).await.unwrap();
        writer.write_at(0, b"abc").await.unwrap();
        writer.finish(&dir.join("done"), None).await.unwrap();
        assert_eq!(std::fs::read(dir.join("done")).unwrap(), b"abc");
    }

    #[tokio::test]
    async fn a_block_past_the_end_is_refused() {
        let dir = dir("past");
        let mut writer = BlockWriter::open(&dir.join("part"), 4, 0).await.unwrap();
        assert!(writer.write_at(2, &[0; 3]).await.is_err());
        assert_eq!(writer.contiguous(), 0);
    }

    /// The offset saved is behind what was written after the last sync:
    /// on reopen what is past it is cut, and what was saved is kept
    /// unless the file is shorter.
    #[tokio::test]
    async fn a_reopen_keeps_the_saved_bytes_or_the_file_s_length_if_shorter() {
        let dir = dir("reopen");
        let part = dir.join("part");
        std::fs::write(&part, vec![7u8; 1000]).unwrap();
        let writer = BlockWriter::open(&part, 2000, 600).await.unwrap();
        assert_eq!(writer.contiguous(), 600);
        assert_eq!(std::fs::metadata(&part).unwrap().len(), 600);
        drop(writer);
        let writer = BlockWriter::open(&part, 2000, 5000).await.unwrap();
        assert_eq!(writer.contiguous(), 600);
    }
}
