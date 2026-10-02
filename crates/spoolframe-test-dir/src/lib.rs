//! A test's own directory, for the tests of every Spoolframe crate and of
//! the Linux client.
//!
//! The name comes from tmpdir: a random prefix, so tests running at once,
//! in this run or another checkout's, never share a directory. Removal is
//! this crate's: tmpdir's own `Drop` only spawns a tokio task, which a
//! `#[tokio::test]` runtime shuts down before it runs (a run of the server
//! suite left some two hundred directories behind) and which panics where
//! there is no runtime at all. A [`TestDir`] removes its directory before
//! `drop` returns, and never lets tmpdir's `Drop` run.

use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::path::{Path, PathBuf};

pub struct TestDir {
    path: PathBuf,
    /// Never dropped: see the crate's docs.
    _tmp: ManuallyDrop<tmpdir::TmpDir>,
}

impl TestDir {
    /// A new, empty directory whose name ends in `name`.
    pub async fn new(name: &str) -> Self {
        let tmp = tmpdir::TmpDir::new(name)
            .await
            .unwrap_or_else(|e| panic!("cannot make a test directory for {name}: {e}"));
        Self {
            path: tmp.to_path_buf(),
            _tmp: ManuallyDrop::new(tmp),
        }
    }

    /// The same, for a test without a tokio runtime (GStreamer's, GLib's,
    /// plain threads). Panics inside one: use [`TestDir::new`] there.
    pub fn new_blocking(name: &str) -> Self {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime to make the test directory")
            .block_on(Self::new(name))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("cannot remove {}: {e}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_directory_is_gone_once_dropped() {
        let dir = TestDir::new("spoolframe-test-dir").await;
        std::fs::write(dir.join("file"), b"x").unwrap();
        let path = dir.to_path_buf();
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn a_test_without_a_runtime_gets_one_too() {
        let dir = TestDir::new_blocking("spoolframe-test-dir-blocking");
        let path = dir.to_path_buf();
        assert!(path.is_dir());
        drop(dir);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn two_of_one_name_are_apart() {
        let (a, b) = (
            TestDir::new("spoolframe-same").await,
            TestDir::new("spoolframe-same").await,
        );
        assert_ne!(a.path(), b.path());
    }
}
