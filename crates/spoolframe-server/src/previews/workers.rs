//! How many files previews are made of ahead at once, by default
//! (`previews.workers`, P3-13): one per physical core the server may use.
//!
//! A tile is one short ffmpeg run, mostly a seek and one decode, so a
//! worker keeps about one core busy; a second hardware thread on the same
//! core adds little to it. The physical cores come from sysfs, where
//! every logical CPU names the CPUs of its core (`core_cpus_list`; older
//! kernels `thread_siblings_list`): one name per core. No crate is needed
//! for that, and reading it ourselves tells a count that is not known
//! apart from one that is (num_cpus' `get_physical` returns the logical
//! count for both), which the settings page and the log say.

use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

/// What the default counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counted {
    /// The machine's physical cores.
    PhysicalCores,
    /// Fewer CPUs than cores are the server's (affinity, a cgroup's
    /// quota): those.
    AvailableCpus,
    /// The cores are not known: the logical CPUs the server may use.
    LogicalCpus,
}

impl Counted {
    /// As the settings page and the log say it.
    pub fn label(self) -> &'static str {
        match self {
            Self::PhysicalCores => "physical cores",
            Self::AvailableCpus => "CPUs it may use",
            Self::LogicalCpus => "logical CPUs",
        }
    }
}

impl std::fmt::Display for Counted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The default and what it counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultWorkers {
    pub count: usize,
    pub counted: Counted,
}

/// The default on this machine, worked out once.
pub fn default_workers() -> DefaultWorkers {
    static ONCE: OnceLock<DefaultWorkers> = OnceLock::new();
    *ONCE.get_or_init(|| {
        choose(
            physical_cores(Path::new("/sys/devices/system/cpu")),
            // Affinity and a cgroup's CPU quota count here.
            std::thread::available_parallelism().map_or(1, |n| n.get()),
        )
    })
}

/// The physical cores, but no more than the `available` CPUs; those when
/// the cores are not known.
pub fn choose(physical: Option<usize>, available: usize) -> DefaultWorkers {
    let available = available.max(1);
    let (count, counted) = match physical.filter(|&p| p > 0) {
        Some(p) if p <= available => (p, Counted::PhysicalCores),
        Some(_) => (available, Counted::AvailableCpus),
        None => (available, Counted::LogicalCpus),
    };
    DefaultWorkers { count, counted }
}

/// The physical cores under `cpus` (sysfs' `devices/system/cpu`): the
/// different sets of CPUs that share a core. `None` when sysfs does not
/// say.
pub fn physical_cores(cpus: &Path) -> Option<usize> {
    let mut cores = HashSet::new();
    for entry in std::fs::read_dir(cpus).ok()?.flatten() {
        let name = entry.file_name();
        let is_cpu = name
            .to_str()
            .and_then(|n| n.strip_prefix("cpu"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !is_cpu {
            continue;
        }
        let topology = entry.path().join("topology");
        // An offline CPU has no topology: not counted.
        let siblings = std::fs::read_to_string(topology.join("core_cpus_list"))
            .or_else(|_| std::fs::read_to_string(topology.join("thread_siblings_list")));
        if let Ok(siblings) = siblings {
            cores.insert(siblings.trim().to_owned());
        }
    }
    (!cores.is_empty()).then_some(cores.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_physical_cores_unless_fewer_cpus_are_the_server_s() {
        assert_eq!(
            choose(Some(8), 16),
            DefaultWorkers {
                count: 8,
                counted: Counted::PhysicalCores
            }
        );
        // Pinned to 4 CPUs, or a quota of 4.
        assert_eq!(
            choose(Some(8), 4),
            DefaultWorkers {
                count: 4,
                counted: Counted::AvailableCpus
            }
        );
        // Cores not known: the logical CPUs it may use.
        assert_eq!(
            choose(None, 12),
            DefaultWorkers {
                count: 12,
                counted: Counted::LogicalCpus
            }
        );
        assert_eq!(choose(Some(0), 0).count, 1);
    }

    /// A sysfs `cpu` directory: each CPU's `core_cpus_list` (or, with
    /// `old`, `thread_siblings_list`), `None` for an offline one.
    fn sysfs(test: &str, cpus: &[Option<&str>], old: bool) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spoolframe-cores-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (n, siblings) in cpus.iter().enumerate() {
            let cpu = dir.join(format!("cpu{n}"));
            std::fs::create_dir_all(&cpu).unwrap();
            if let Some(siblings) = siblings {
                std::fs::create_dir_all(cpu.join("topology")).unwrap();
                let file = if old {
                    "thread_siblings_list"
                } else {
                    "core_cpus_list"
                };
                std::fs::write(cpu.join("topology").join(file), format!("{siblings}\n")).unwrap();
            }
        }
        // Not CPUs.
        std::fs::create_dir_all(dir.join("cpufreq")).unwrap();
        std::fs::create_dir_all(dir.join("cpuidle")).unwrap();
        dir
    }

    #[test]
    fn two_threads_of_a_core_are_one_core() {
        // 4 cores of 2 threads, numbered as Linux numbers them on x86.
        let dir = sysfs(
            "smt",
            &[
                Some("0,4"),
                Some("1,5"),
                Some("2,6"),
                Some("3,7"),
                Some("0,4"),
                Some("1,5"),
                Some("2,6"),
                Some("3,7"),
            ],
            false,
        );
        assert_eq!(physical_cores(&dir), Some(4));
        std::fs::remove_dir_all(&dir).unwrap();

        // An older kernel, and one CPU offline.
        let dir = sysfs("old", &[Some("0-1"), Some("0-1"), Some("2-3"), None], true);
        assert_eq!(physical_cores(&dir), Some(2));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_topology_is_no_count() {
        let dir = sysfs("none", &[None, None], false);
        assert_eq!(physical_cores(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(physical_cores(Path::new("/nonexistent/cpu")), None);
    }
}
