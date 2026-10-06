//! Opening files that someone other than the server can change: media
//! and the subtitle files beside them (PX-09 B-1). And keeping the
//! server's own files to its own user (PX-11 finding 35).

use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::Path;

/// `dir`, made if missing, as only the server's user may enter it
/// (PX-11 finding 35): a cache holds what users watch. One an older
/// server made open to everyone is closed.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// `path`, made empty if missing, as only the server's user may read or
/// write it; one made open to everyone is closed.
pub fn private_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// `path` opened to read without waiting: a FIFO swapped in for a file
/// does not hold the read until something writes to it.
pub async fn open_nonblocking(path: &Path) -> std::io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .await
}

/// `path` opened to read if it is a regular file itself: not through a
/// symlink, which a consumer who can write to a library folder could point
/// anywhere the server can read, and not a FIFO or a device. A subtitle
/// file may be replaced during its playback (P2-11), so it is checked on
/// every open rather than pinned at the start.
pub async fn open_regular(path: &Path) -> std::io::Result<tokio::fs::File> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .await?;
    if !file.metadata().await?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

/// Whether `path` is a regular file itself, not a symlink, before a path
/// is handed to ffmpeg, which follows links. A swap after this check still
/// reaches ffmpeg; what it then reads is parsed as a subtitle, not served.
pub async fn is_regular(path: &Path) -> std::io::Result<()> {
    if tokio::fs::symlink_metadata(path).await?.is_file() {
        Ok(())
    } else {
        Err(std::io::Error::other("not a regular file"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_symlink_to_a_regular_file_is_not_opened() {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-files-symlink").await;
        let dir = tmp.as_ref();
        std::fs::write(dir.join("secret"), b"key").unwrap();
        std::os::unix::fs::symlink(dir.join("secret"), dir.join("Movie.srt")).unwrap();
        assert!(open_regular(&dir.join("Movie.srt")).await.is_err());
        assert!(is_regular(&dir.join("Movie.srt")).await.is_err());
        assert!(open_regular(&dir.join("secret")).await.is_ok());
    }
}
