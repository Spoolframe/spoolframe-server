//! Free space where the downloads are: the file system's own figure.

use std::path::Path;

/// The bytes an unprivileged user may still write on the file system that
/// holds `path` (statvfs's `f_bavail` blocks of `f_frsize`).
#[cfg(unix)]
pub(crate) fn available(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` is a NUL-terminated string that outlives the call,
    // and `stat` points at memory for one `statvfs`, which the call
    // fills in.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so it wrote the whole struct (and it
    // was zeroed before).
    let stat = unsafe { stat.assume_init() };
    // Both are u64 on Linux; f_bavail is u32 on Apple platforms.
    #[allow(clippy::useless_conversion)]
    Ok(u64::from(stat.f_bavail).saturating_mul(u64::from(stat.f_frsize)))
}

#[cfg(not(unix))]
pub(crate) fn available(_path: &Path) -> std::io::Result<u64> {
    Err(std::io::Error::other(
        "free space is not known on this platform: set it with Downloads::set_free_space",
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_temporary_directory_has_some_room() {
        let free = super::available(&std::env::temp_dir()).unwrap();
        assert!(free > 0);
    }

    #[test]
    fn a_path_that_is_not_there_is_an_error() {
        assert!(super::available(std::path::Path::new("/nonexistent/sf-downloads")).is_err());
    }
}
