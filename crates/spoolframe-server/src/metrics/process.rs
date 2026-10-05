//! The server process itself: memory, open files. Read from `/proc`, so
//! only on Linux; elsewhere they are left out rather than reported as 0.

/// The process's resident memory, in bytes.
pub fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf only reads a constant of the system's.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(page).ok().map(|page| pages * page)
}

/// How many files (sockets, pipes, …) the process has open.
pub fn open_files() -> Option<u64> {
    let entries = std::fs::read_dir("/proc/self/fd").ok()?;
    // The directory being read is one of them.
    Some((entries.count() as u64).saturating_sub(1))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn memory_and_open_files_are_read() {
        assert!(resident_bytes().unwrap() > 0);
        // Other tests open and close files at the same time: only that
        // the count includes this one is certain.
        let _file = std::fs::File::open("/proc/self/statm").unwrap();
        assert!(open_files().unwrap() >= 1);
    }
}
