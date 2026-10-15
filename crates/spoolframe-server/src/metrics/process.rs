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

/// The resident memory by what it is, in bytes: the parts add up to
/// [`resident_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResidentParts {
    /// Anonymous memory: the heap, stacks, and the caches' memory tiers.
    pub heap: u64,
    /// Pages of files mapped in: the binary and its libraries, mostly.
    pub files: u64,
    /// Shared memory.
    pub shared: u64,
}

/// The resident memory's parts, from `/proc/self/status`.
pub fn resident_parts() -> Option<ResidentParts> {
    parse_status(&std::fs::read_to_string("/proc/self/status").ok()?)
}

/// `RssAnon`, `RssFile` and `RssShmem` of a `/proc/<pid>/status`, which
/// the kernel gives in kB.
fn parse_status(status: &str) -> Option<ResidentParts> {
    let field = |name: &str| -> Option<u64> {
        let line = status
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))?;
        let kb: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
        Some(kb * 1024)
    };
    Some(ResidentParts {
        heap: field("RssAnon")?,
        files: field("RssFile")?,
        shared: field("RssShmem")?,
    })
}

/// How many files (sockets, pipes, …) the process has open.
pub fn open_files() -> Option<u64> {
    let entries = std::fs::read_dir("/proc/self/fd").ok()?;
    // The directory being read is one of them.
    Some((entries.count() as u64).saturating_sub(1))
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn the_parts_are_read_from_status_in_bytes() {
        let status = "Name:\tspoolframe-serv\nVmRSS:\t  61440 kB\n\
                      RssAnon:\t   40960 kB\nRssFile:\t   18432 kB\nRssShmem:\t    2048 kB\n";
        assert_eq!(
            parse_status(status),
            Some(ResidentParts {
                heap: 40 << 20,
                files: 18 << 20,
                shared: 2 << 20,
            })
        );
        // A kernel without the split (before 4.5) has no parts.
        assert_eq!(parse_status("VmRSS:\t  61440 kB\n"), None);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn memory_and_open_files_are_read() {
        assert!(resident_bytes().unwrap() > 0);
        assert!(resident_parts().unwrap().heap > 0);
        // Other tests open and close files at the same time: only that
        // the count includes this one is certain.
        let _file = std::fs::File::open("/proc/self/statm").unwrap();
        assert!(open_files().unwrap() >= 1);
    }
}
