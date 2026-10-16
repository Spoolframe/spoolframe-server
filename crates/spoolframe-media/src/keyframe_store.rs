//! Keyframe lists that took a scan of every packet (PX-25), kept on disk
//! so a restart does not read the file again.
//!
//! A file with no index the reader follows (a transport stream, `.ts` or
//! `.m2ts`) has its keyframes listed by reading all of it: minutes for a
//! large remux, longer than an app waits for a playback to start. The
//! prober keeps that list in memory; this keeps it across restarts.
//!
//! One file per media path, named by a hash of the path, holding the
//! path, the media file's size and modification time when it was read,
//! and the list. A list whose file has changed since is not used, and is
//! replaced by the next scan. A file that is gone has its list removed
//! when it is next asked for.
//!
//! The format, little-endian: [`MAGIC`], the path's length (u32) and
//! bytes, the size (u64), the modification time in ns since the epoch
//! (u128), the first keyframe's own time (i64, ms), the count (u64), and
//! each time as the ms since the one before (u32).

use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::keyframe_index::MAX_KEYFRAMES;
use crate::probe::KeyframeTimes;

/// What a list file starts with; a new layout is a new number.
const MAGIC: &[u8; 8] = b"SFKEYS01";

/// The longest path kept: Linux's `PATH_MAX`.
const MAX_PATH: usize = 4096;

/// The media file as it was when its keyframes were read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Made {
    pub size: u64,
    /// ns since the epoch.
    pub modified: u128,
}

impl Made {
    /// `None` without a modification time: a list could not be told from
    /// one of a changed file.
    pub fn of(size: u64, modified: Option<SystemTime>) -> Option<Self> {
        let modified = modified?
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some(Self { size, modified })
    }
}

/// Lists kept in a directory of the server's cache.
#[derive(Debug, Clone)]
pub struct KeyframeStore {
    dir: PathBuf,
}

impl KeyframeStore {
    /// Lists kept in `dir`, made if missing, only the server's user may
    /// read it.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
        })
    }

    /// Where `path`'s list is kept.
    fn file(&self, path: &Path) -> PathBuf {
        let hash = blake3::hash(path.as_os_str().as_bytes());
        self.dir.join(&hash.to_hex()[..32])
    }

    /// `path`'s list, if one is kept for the file as `made`.
    pub(crate) fn get(&self, path: &Path, made: Made) -> Option<KeyframeTimes> {
        let bytes = std::fs::read(self.file(path)).ok()?;
        parse(&bytes, path, made)
    }

    /// Whether a list is kept for `path` as `made`, reading only its head.
    pub(crate) fn has(&self, path: &Path, made: Made) -> bool {
        use std::io::Read as _;
        let Ok(file) = std::fs::File::open(self.file(path)) else {
            return false;
        };
        let mut head = Vec::new();
        let most = head_len(path.as_os_str().len()) as u64;
        if file.take(most).read_to_end(&mut head).is_err() {
            return false;
        }
        read_head(&head, path, made).is_some()
    }

    /// Keep `times` as `path`'s list for the file as `made`: written aside
    /// and renamed over the old one, so a list is never read half written.
    pub(crate) fn put(
        &self,
        path: &Path,
        made: Made,
        times: &KeyframeTimes,
    ) -> std::io::Result<()> {
        let bytes = encode(path, made, times)?;
        let file = self.file(path);
        let aside = file.with_extension("new");
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&aside)?;
        out.write_all(&bytes)?;
        out.sync_all()?;
        drop(out);
        std::fs::rename(&aside, &file)
    }

    /// Drop `path`'s list: its file is gone.
    pub(crate) fn forget(&self, path: &Path) {
        let _ = std::fs::remove_file(self.file(path));
    }
}

/// The head's length for a path of `path_len` bytes.
fn head_len(path_len: usize) -> usize {
    MAGIC.len() + 4 + path_len + 8 + 16
}

fn encode(path: &Path, made: Made, times: &KeyframeTimes) -> std::io::Result<Vec<u8>> {
    let path = path.as_os_str().as_bytes();
    if path.len() > MAX_PATH || times.times_ms.len() > MAX_KEYFRAMES {
        return Err(std::io::Error::other("too large to keep"));
    }
    let mut out = Vec::with_capacity(head_len(path.len()) + 16 + times.times_ms.len() * 4);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(path.len() as u32).to_le_bytes());
    out.extend_from_slice(path);
    out.extend_from_slice(&made.size.to_le_bytes());
    out.extend_from_slice(&made.modified.to_le_bytes());
    out.extend_from_slice(&times.first_ms.to_le_bytes());
    out.extend_from_slice(&(times.times_ms.len() as u64).to_le_bytes());
    let mut before = 0;
    for &t in &times.times_ms {
        let step = t
            .checked_sub(before)
            .and_then(|s| u32::try_from(s).ok())
            .ok_or_else(|| std::io::Error::other("keyframes out of order"))?;
        out.extend_from_slice(&step.to_le_bytes());
        before = t;
    }
    Ok(out)
}

/// The bytes after the head, if it names `path` as `made`.
fn read_head<'a>(bytes: &'a [u8], path: &Path, made: Made) -> Option<&'a [u8]> {
    let path = path.as_os_str().as_bytes();
    let mut r = Reader(bytes);
    (r.take(MAGIC.len())? == MAGIC).then_some(())?;
    let len = u32::from_le_bytes(r.array()?) as usize;
    (len == path.len() && r.take(len)? == path).then_some(())?;
    let size = u64::from_le_bytes(r.array()?);
    let modified = u128::from_le_bytes(r.array()?);
    (Made { size, modified } == made).then_some(r.0)
}

/// The list in `bytes`, if they are one, of `path` as `made`.
fn parse(bytes: &[u8], path: &Path, made: Made) -> Option<KeyframeTimes> {
    let mut r = Reader(read_head(bytes, path, made)?);
    let first_ms = i64::from_le_bytes(r.array()?);
    let count = u64::from_le_bytes(r.array()?);
    let count = usize::try_from(count)
        .ok()
        .filter(|&c| c <= MAX_KEYFRAMES)?;
    // Exactly the count: a file cut short or with more is not one.
    (r.0.len() == count * 4).then_some(())?;
    let mut times = Vec::with_capacity(count);
    let mut at = 0u64;
    for _ in 0..count {
        at += u64::from(u32::from_le_bytes(r.array()?));
        times.push(at);
    }
    Some(KeyframeTimes {
        times_ms: times,
        first_ms,
    })
}

/// Bytes read from the front.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        (self.0.len() >= n).then(|| {
            let (head, rest) = self.0.split_at(n);
            self.0 = rest;
            head
        })
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn made() -> Made {
        Made {
            size: 58_000_000_000,
            modified: 1_760_000_000_123_456_789,
        }
    }

    fn times() -> KeyframeTimes {
        KeyframeTimes {
            times_ms: vec![0, 1001, 2002, 5005],
            first_ms: -40,
        }
    }

    #[test]
    fn a_list_reads_back_as_it_was_written() {
        let path = Path::new("/media/Predators.2010.m2ts");
        let bytes = encode(path, made(), &times()).unwrap();
        assert_eq!(parse(&bytes, path, made()), Some(times()));
    }

    #[test]
    fn a_list_of_another_file_or_another_version_of_it_is_not_used() {
        let path = Path::new("/media/a.ts");
        let bytes = encode(path, made(), &times()).unwrap();
        assert!(parse(&bytes, Path::new("/media/b.ts"), made()).is_none());
        let grown = Made {
            size: made().size + 1,
            ..made()
        };
        assert!(parse(&bytes, path, grown).is_none());
        let touched = Made {
            modified: made().modified + 1,
            ..made()
        };
        assert!(parse(&bytes, path, touched).is_none());
    }

    #[test]
    fn a_list_cut_short_or_longer_is_not_used() {
        let path = Path::new("/media/a.ts");
        let mut bytes = encode(path, made(), &times()).unwrap();
        assert!(parse(&bytes[..bytes.len() - 1], path, made()).is_none());
        bytes.push(0);
        assert!(parse(&bytes, path, made()).is_none());
    }

    #[test]
    fn a_store_keeps_one_list_per_path_and_forgets_it() {
        let dir =
            std::env::temp_dir().join(format!("spoolframe-keyframe-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = KeyframeStore::open(&dir).unwrap();
        let path = Path::new("/media/a.ts");
        assert!(!store.has(path, made()));
        store.put(path, made(), &times()).unwrap();
        assert!(store.has(path, made()));
        assert_eq!(store.get(path, made()), Some(times()));
        let changed = Made { size: 1, ..made() };
        store.put(path, changed, &times()).unwrap();
        assert!(!store.has(path, made()));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        store.forget(path);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
