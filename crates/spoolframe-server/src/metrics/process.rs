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

/// The bytes the server's allocations hold now, as asked for, counted by
/// its global allocator (`main.rs`). Not a part of the resident heap: a
/// block never written to has no pages yet, so this can be the larger.
/// What it tells is the heap's trend: a heap that stays high while this
/// falls back is memory the allocator keeps for reuse, not a leak.
pub static ALLOCATED: Allocated = Allocated::new();

/// Slots of [`Allocated`]: a thread counts in one of them, so threads
/// allocating at once seldom write the same cache line.
const SLOTS: usize = 32;

/// A count of bytes split over [`SLOTS`]. A block freed on another
/// thread than the one that made it takes its bytes from that thread's
/// slot, so a slot may wrap below zero; the sum does not.
pub struct Allocated {
    slots: [Slot; SLOTS],
}

#[repr(align(128))]
struct Slot(std::sync::atomic::AtomicU64);

impl Allocated {
    const fn new() -> Self {
        Self {
            slots: [const { Slot(std::sync::atomic::AtomicU64::new(0)) }; SLOTS],
        }
    }

    /// `bytes` more held. Called from the global allocator, so it must
    /// not allocate: the thread's slot is a `const` thread local, which
    /// neither allocates nor has a destructor.
    pub fn add(&self, bytes: usize) {
        self.slot()
            .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// `bytes` fewer held.
    pub fn sub(&self, bytes: usize) {
        self.slot()
            .fetch_sub(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// The bytes held; 0 where nothing counts them (the tests' allocator).
    pub fn bytes(&self) -> u64 {
        let sum = self.slots.iter().fold(0u64, |sum, slot| {
            sum.wrapping_add(slot.0.load(std::sync::atomic::Ordering::Relaxed))
        });
        // The slots are read one at a time, so a free can be seen without
        // the allocation it frees: below zero, read as none.
        u64::try_from(sum as i64).unwrap_or(0)
    }

    fn slot(&self) -> &std::sync::atomic::AtomicU64 {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        thread_local! {
            static SLOT: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
        }
        // A thread being torn down has no thread locals left: slot 0.
        let i = SLOT
            .try_with(|slot| {
                if slot.get() == usize::MAX {
                    slot.set(NEXT.fetch_add(1, Ordering::Relaxed) % SLOTS);
                }
                slot.get()
            })
            .unwrap_or(0);
        &self.slots[i].0
    }
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

    #[test]
    fn blocks_freed_on_other_threads_are_taken_off_the_sum() {
        let allocated = Allocated::new();
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| allocated.add(1000));
            }
        });
        // Freed where none of them were made.
        std::thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| allocated.sub(1000));
            }
        });
        assert_eq!(allocated.bytes(), 5000);
    }

    #[test]
    fn a_free_seen_before_its_allocation_reads_as_none() {
        let allocated = Allocated::new();
        allocated.sub(10);
        assert_eq!(allocated.bytes(), 0);
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
