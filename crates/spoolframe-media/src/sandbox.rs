//! ffmpeg, ffprobe and tesseract confined (PX-11 finding 34, PX-09 B-12).
//!
//! They parse files anyone who can write to a library folder made, in C.
//! So each run gets only what it needs, applied to the child between fork
//! and exec, with no namespace, so the service's own sandbox needs none:
//!
//! - **Landlock**, when [`Mode`] lets it confine: the system's programs and
//!   libraries (the nix store, `/usr`, …) to read and run; each input file
//!   to read, by the inode the server opened and checked; the directories
//!   the run was given, to read or write. Nothing else: not the state
//!   directory, not systemd's credentials, not `/home`, not `/proc`. No TCP
//!   connect or bind (ABI 4), no signal or abstract Unix socket out of the
//!   run (ABI 6).
//! - **seccomp**, unless `off`: no new processes (threads only), no sockets
//!   but Unix ones and no `connect` at all, and none of the calls an escape
//!   uses: `ptrace`, `mount`, namespaces, keyrings, `bpf`, `io_uring`, …
//! - **always**, confined or not: a clean environment (a short allowlist,
//!   and `HOME` and `TMPDIR` in the job's directory), resource limits
//!   (memory, CPU time, file size, no core dumps), and a process group of
//!   its own, so a signal reaches the whole run.
//!
//! Everything is made before the fork: the ruleset (its file descriptor
//! holds the rules) and the BPF. After the fork the child only calls
//! setrlimit(2), prctl(2) and landlock_restrict_self(2), and allocates
//! nothing: the server is multithreaded, and another thread may have held
//! the allocator's lock when it forked.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use landlock::{
    ABI, Access as _, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible as _, PathBeneath,
    Ruleset, RulesetAttr as _, RulesetCreated, RulesetCreatedAttr as _, RulesetStatus, Scope,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// `media.sandbox`: whether the tools must be confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Landlock and seccomp, or the server does not start.
    Required,
    /// Landlock and seccomp if this kernel has Landlock; otherwise seccomp
    /// alone, said loudly.
    Auto,
    /// Never confined. The environment and the limits still apply.
    Off,
}

impl std::str::FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "required" => Ok(Self::Required),
            "auto" => Ok(Self::Auto),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "{other:?}: one of \"required\", \"auto\" or \"off\""
            )),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("media.sandbox is \"required\", but the media tools cannot be confined here: {reason}")]
    Unavailable { reason: String },
}

/// The oldest Landlock ABI that confines a run: 3 (Linux 6.2) is the first
/// to refuse truncate(2) outside the rules, without which a run could
/// empty the database through its path.
pub const MIN_LANDLOCK_ABI: i32 = 3;

/// How the tools run: confined by Landlock and seccomp, by seccomp alone,
/// or not at all. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Sandbox(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    /// Landlock's ABI on this kernel, when runs are confined by it.
    landlock: Option<i32>,
    /// The seccomp filters, unless `off`.
    seccomp: Option<Arc<Filters>>,
    /// No input is read from in here: the state directory, with the
    /// database, the key and the secrets; systemd's credentials.
    hidden: Vec<PathBuf>,
    /// The GPU's device nodes, for a run that asks for them.
    gpu: Vec<PathBuf>,
}

/// The seccomp filters, as BPF, in the order they are loaded.
#[derive(Debug)]
struct Filters(Vec<BpfProgram>);

impl Sandbox {
    /// Nothing confined: for tests with stand-in programs, and
    /// `media.sandbox = "off"`.
    pub fn off() -> Self {
        Self(Arc::new(Inner {
            landlock: None,
            seccomp: None,
            hidden: Vec::new(),
            gpu: Vec::new(),
        }))
    }

    /// Confine as `mode` says, with this kernel's Landlock. Without it (or
    /// with too old a one), `Required` is an error and `Auto` runs with
    /// seccomp alone and logs why. No input of a run may be inside
    /// `hidden`, and a run that uses the GPU gets the device nodes in
    /// `gpu`.
    pub fn new(mode: Mode, hidden: Vec<PathBuf>, gpu: Vec<PathBuf>) -> Result<Self, SandboxError> {
        Self::with_landlock(mode, landlock_abi(), hidden, gpu)
    }

    /// [`Sandbox::new`] with the kernel's answer given: Landlock's ABI, or
    /// why there is none.
    fn with_landlock(
        mode: Mode,
        abi: Result<i32, String>,
        hidden: Vec<PathBuf>,
        gpu: Vec<PathBuf>,
    ) -> Result<Self, SandboxError> {
        let hidden = hidden
            .into_iter()
            .map(|dir| std::fs::canonicalize(&dir).unwrap_or(dir))
            .collect();
        let mut inner = Inner {
            landlock: None,
            seccomp: None,
            hidden,
            gpu,
        };
        if mode == Mode::Off {
            tracing::warn!(
                "media.sandbox is \"off\": ffmpeg, ffprobe and tesseract run with the server's \
                 own access to files and the network"
            );
            return Ok(Self(Arc::new(inner)));
        }
        let seccomp = filters();
        let landlock = abi.and_then(|abi| {
            if abi < MIN_LANDLOCK_ABI {
                return Err(format!(
                    "this kernel's Landlock is ABI {abi}; confining needs ABI \
                     {MIN_LANDLOCK_ABI} (Linux 6.2) or later"
                ));
            }
            // The calls a run makes before it starts, but the last: a
            // seccomp filter (systemd's SystemCallFilter) may refuse them.
            ruleset(abi, &inner, None, &Jail::new(Limits::PROBE))
                .map_err(|e| format!("cannot make a Landlock ruleset: {e}"))?;
            Ok(abi)
        });
        match (landlock, seccomp) {
            (Ok(abi), Ok(filters)) => {
                tracing::info!(
                    landlock_abi = abi,
                    "media tools run confined: Landlock ABI {abi} and a seccomp filter"
                );
                inner.landlock = Some(abi);
                inner.seccomp = Some(Arc::new(filters));
            }
            (Err(reason), _) | (_, Err(reason)) if mode == Mode::Required => {
                return Err(SandboxError::Unavailable { reason });
            }
            (Err(reason), seccomp) => {
                tracing::error!(
                    reason,
                    "MEDIA TOOLS RUN WITHOUT LANDLOCK: ffmpeg, ffprobe and tesseract parse \
                     untrusted files with the server's own access to its state directory, \
                     limited only by a seccomp filter and resource limits. Use Linux 6.2 or \
                     later with Landlock enabled (lsm=…,landlock), or set media.sandbox = \
                     \"required\" to refuse to start without it"
                );
                match seccomp {
                    Ok(filters) => inner.seccomp = Some(Arc::new(filters)),
                    Err(reason) => tracing::error!(reason, "media tools run without seccomp"),
                }
            }
            (Ok(abi), Err(reason)) => {
                tracing::error!(
                    reason,
                    landlock_abi = abi,
                    "MEDIA TOOLS RUN WITHOUT SECCOMP: there is no seccomp filter for this \
                     machine, so ffmpeg, ffprobe and tesseract may start processes and open \
                     sockets; Landlock ABI {abi} still limits their files"
                );
                inner.landlock = Some(abi);
            }
        }
        Ok(Self(Arc::new(inner)))
    }

    /// Whether runs are confined by Landlock.
    pub fn confines(&self) -> bool {
        self.0.landlock.is_some()
    }

    /// The Landlock ABI runs are confined with.
    pub fn landlock_abi(&self) -> Option<i32> {
        self.0.landlock
    }
}

/// This kernel's Landlock ABI, or why it has none.
fn landlock_abi() -> Result<i32, String> {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: landlock_create_ruleset(2) with no attributes and the version
    // flag only reads its arguments.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi >= 0 {
        return i32::try_from(abi).map_err(|_| format!("Landlock ABI {abi}?"));
    }
    let error = std::io::Error::last_os_error();
    Err(match error.raw_os_error() {
        Some(libc::ENOSYS) => "this kernel has no Landlock".to_owned(),
        Some(libc::EOPNOTSUPP) => {
            "Landlock is not enabled on this kernel (add it to the lsm= boot parameter)".to_owned()
        }
        Some(libc::EPERM) => format!(
            "landlock_create_ruleset(2) is refused, by a seccomp filter such as systemd's \
             SystemCallFilter: {error}"
        ),
        _ => format!("landlock_create_ruleset(2): {error}"),
    })
}

/// The GPU device nodes a host may encode or decode on: VA-API's render
/// node, and NVIDIA's nodes when `nvidia`. Only those that exist.
pub fn gpu_devices(vaapi: Option<&Path>, nvidia: bool) -> Vec<PathBuf> {
    let mut devices: Vec<PathBuf> = vaapi.map(Path::to_path_buf).into_iter().collect();
    if nvidia {
        for name in [
            "nvidiactl",
            "nvidia-uvm",
            "nvidia-uvm-tools",
            "nvidia-modeset",
            "nvidia-caps",
        ] {
            devices.push(Path::new("/dev").join(name));
        }
        if let Ok(dir) = std::fs::read_dir("/dev") {
            for entry in dir.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name
                    .strip_prefix("nvidia")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                {
                    devices.push(entry.path());
                }
            }
        }
    }
    devices.retain(|d| d.exists());
    devices
}

/// What a run may use, and how much.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `RLIMIT_DATA`: heap, private mappings and thread stacks.
    pub memory: Memory,
    /// `RLIMIT_CPU`: seconds of CPU, every thread's added up.
    pub cpu_secs: u64,
    /// `RLIMIT_FSIZE`: the largest file it may write, bytes.
    pub file_size: u64,
}

/// A memory limit that grows with the machine: ffmpeg starts a thread or
/// more a core, and `RLIMIT_DATA` counts each one's stack and buffers as
/// soon as they are mapped, used or not. Measured on 32 cores (PX-11
/// finding 34): a 4K x264 encode needs 8 to 12 GiB of it, one 4K HEVC
/// tile 3 to 4 GiB, ffprobe on the same file under 1 GiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    pub base: u64,
    pub per_core: u64,
}

impl Memory {
    /// Exactly `bytes`, whatever the machine.
    pub const fn fixed(bytes: u64) -> Self {
        Self {
            base: bytes,
            per_core: 0,
        }
    }

    /// The bytes on this machine.
    pub fn bytes(self) -> u64 {
        static CORES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let cores = *CORES
            .get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get() as u64));
        self.base
            .saturating_add(self.per_core.saturating_mul(cores))
    }
}

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

impl Limits {
    /// ffprobe reading a file's headers, or every packet of one stream.
    pub const PROBE: Self = Self {
        memory: Memory {
            base: GIB,
            per_core: 32 * MIB,
        },
        cpu_secs: 20 * 60,
        file_size: MIB,
    };
    /// An ffmpeg pass over a whole film that decodes no picture: a track
    /// copied out or converted.
    pub const PASS: Self = Self {
        memory: Memory {
            base: 2 * GIB,
            per_core: 32 * MIB,
        },
        cpu_secs: 2 * 60 * 60,
        file_size: 512 * MIB,
    };
    /// One picture decoded and written small: a preview tile.
    pub const TILE: Self = Self {
        memory: Memory {
            base: GIB,
            per_core: 192 * MIB,
        },
        cpu_secs: 10 * 60,
        file_size: 16 * MIB,
    };
    /// A film decoded and encoded, up to 4K, for as long as it plays; the
    /// file size is the caller's, which knows how much may be written.
    pub const fn encode(file_size: u64) -> Self {
        Self {
            memory: Memory {
                base: 2 * GIB,
                per_core: 512 * MIB,
            },
            cpu_secs: 72 * 60 * 60,
            file_size,
        }
    }
    /// One tesseract run over a few dozen cue pictures, on one thread.
    pub const OCR: Self = Self {
        memory: Memory::fixed(2 * GIB),
        cpu_secs: 15 * 60,
        file_size: MIB,
    };
}

/// One run's view of the file system: the inputs it reads, the
/// directories it reads or writes, whether it uses the GPU, and its
/// [`Limits`].
#[derive(Debug, Clone)]
pub struct Jail {
    inputs: Vec<PathBuf>,
    reads: Vec<PathBuf>,
    writes: Vec<PathBuf>,
    gpu: bool,
    limits: Limits,
}

impl Jail {
    pub fn new(limits: Limits) -> Self {
        Self {
            inputs: Vec::new(),
            reads: Vec::new(),
            writes: Vec::new(),
            gpu: false,
            limits,
        }
    }

    /// A file the run reads that someone else may have put there: media,
    /// a subtitle beside it. Opened by the server, following symlinks, and
    /// refused unless a regular file outside the hidden directories; the
    /// run may read that inode only, so a file swapped in after the check
    /// is not readable. One that does not exist is left out, and the
    /// program says so as it would anyway.
    pub fn input(mut self, path: impl Into<PathBuf>) -> Self {
        self.inputs.push(path.into());
        self
    }

    /// A directory of the server's own the run reads.
    pub fn reads(mut self, dir: impl Into<PathBuf>) -> Self {
        self.reads.push(dir.into());
        self
    }

    /// A directory of the server's own the run writes: the only writable
    /// path it has.
    pub fn writes(mut self, dir: impl Into<PathBuf>) -> Self {
        self.writes.push(dir.into());
        self
    }

    /// The run may decode or encode on the GPU.
    pub fn gpu(mut self, gpu: bool) -> Self {
        self.gpu = gpu;
        self
    }
}

/// A program and how it runs.
#[derive(Debug, Clone)]
pub struct Tool {
    program: PathBuf,
    sandbox: Sandbox,
}

impl Tool {
    /// `program` (a path, or a name looked up in `PATH`), run in `sandbox`.
    pub fn new(program: impl Into<PathBuf>, sandbox: Sandbox) -> Self {
        Self {
            program: program.into(),
            sandbox,
        }
    }

    /// `program`, never confined: stand-ins in tests.
    pub fn unconfined(program: impl Into<PathBuf>) -> Self {
        Self::new(program, Sandbox::off())
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// A command running the program in `jail`, to add the program's
    /// arguments to. An input that may not be read is an error
    /// (`PermissionDenied`).
    pub fn command(&self, jail: &Jail) -> std::io::Result<Command> {
        let sandbox = &*self.sandbox.0;
        let (program, ruleset) = match sandbox.landlock {
            Some(abi) => {
                // Run by its real path, which the rules name.
                let program = resolve(&self.program)?;
                let ruleset = ruleset(abi, sandbox, Some(&program), jail)?;
                (program, Some(ruleset))
            }
            None => {
                if sandbox.seccomp.is_some() {
                    for input in &jail.inputs {
                        open_input(input, sandbox, jail)?;
                    }
                }
                (self.program.clone(), None)
            }
        };
        let mut command = std::process::Command::new(&program);
        command.env_clear();
        for (key, value) in environment(jail) {
            command.env(key, value);
        }
        command.process_group(0);
        let limits = jail.limits;
        let memory = limits.memory.bytes();
        let seccomp = sandbox.seccomp.clone();
        let mut ruleset = ruleset;
        // SAFETY: between fork and exec only getrlimit(2), setrlimit(2),
        // prctl(2) and landlock_restrict_self(2) run, which are
        // async-signal-safe, and close(2) of the ruleset. Nothing
        // allocates: the ruleset and the filters were made before the
        // fork, the errors are errno values, and `take` moves the ruleset
        // out of the child's copy of the closure without touching the heap.
        unsafe {
            command.pre_exec(move || {
                lower(libc::RLIMIT_DATA, memory)?;
                lower(libc::RLIMIT_CPU, limits.cpu_secs)?;
                lower(libc::RLIMIT_FSIZE, limits.file_size)?;
                lower(libc::RLIMIT_CORE, 0)?;
                if let Some(ruleset) = ruleset.take() {
                    restrict(ruleset)?;
                }
                if let Some(filters) = &seccomp {
                    install(filters)?;
                }
                Ok(())
            });
        }
        Ok(Command {
            command: tokio::process::Command::from(command),
        })
    }

    /// Shows that this tool, run as ffprobe, is confined: it can neither
    /// read `secret` (a directory no run is given: the state directory)
    /// nor connect to a listener on loopback. What it showed, for the log;
    /// or what got through.
    pub async fn check_confined(&self, secret: &Path) -> Result<String, String> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.set_nonblocking(true).map(|()| l))
            .map_err(|e| format!("cannot listen on loopback to check with: {e}"))?;
        let address = listener
            .local_addr()
            .map_err(|e| format!("cannot listen on loopback to check with: {e}"))?;
        let read = self.check_run(secret.as_os_str().to_owned()).await?;
        if !read.contains("Permission denied") {
            return Err(format!(
                "{} could read {}: {read}",
                self.program.display(),
                secret.display()
            ));
        }
        // One second to give up waiting for data, had it connected.
        let url = format!("tcp://{address}?timeout=1000000");
        let connect = self.check_run(url.into()).await?;
        match listener.accept() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            accepted => {
                return Err(format!(
                    "{} connected to {address}: {accepted:?}; it said {connect}",
                    self.program.display()
                ));
            }
        }
        Ok(format!(
            "a confined {} could not read {} nor connect to {address}",
            self.program.display(),
            secret.display()
        ))
    }

    /// ffprobe on `target`, which must fail: what it said.
    async fn check_run(&self, target: OsString) -> Result<String, String> {
        let mut command = self
            .command(&Jail::new(Limits::PROBE))
            .map_err(|e| format!("cannot run {}: {e}", self.program.display()))?;
        command
            .args(["-v", "error", "-i"])
            .arg(&target)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(30), command.output())
            .await
            .map_err(|_| format!("{} did not end", self.program.display()))?
            .map_err(|e| format!("cannot run {}: {e}", self.program.display()))?;
        let said = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if output.status.success() {
            return Err(format!(
                "{} read {}: {said}",
                self.program.display(),
                target.to_string_lossy()
            ));
        }
        Ok(said)
    }
}

/// A command from [`Tool::command`]. It holds what the child confines
/// itself with until it is dropped, so it must outlive its spawn.
#[derive(Debug)]
pub struct Command {
    command: tokio::process::Command,
}

impl std::ops::Deref for Command {
    type Target = tokio::process::Command;

    fn deref(&self) -> &Self::Target {
        &self.command
    }
}

impl std::ops::DerefMut for Command {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.command
    }
}

#[cfg(target_env = "gnu")]
type Resource = libc::__rlimit_resource_t;
#[cfg(not(target_env = "gnu"))]
type Resource = libc::c_int;

/// Sets `resource` to at most `value`, soft and hard, so the program
/// cannot raise it again. A limit already lower stays.
fn lower(resource: Resource, value: u64) -> std::io::Result<()> {
    let mut now = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) and setrlimit(2) with a valid pointer.
    unsafe {
        if libc::getrlimit(resource, &mut now) == -1 {
            return Err(std::io::Error::last_os_error());
        }
        let value = libc::rlim_t::try_from(value)
            .unwrap_or(libc::RLIM_INFINITY)
            .min(now.rlim_max);
        let new = libc::rlimit {
            rlim_cur: value.min(now.rlim_cur),
            rlim_max: value,
        };
        if libc::setrlimit(resource, &new) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The child confines itself to `ruleset`. After the fork: see
/// [`Tool::command`].
fn restrict(ruleset: RulesetCreated) -> std::io::Result<()> {
    match ruleset.restrict_self() {
        Ok(status) if status.ruleset == RulesetStatus::FullyEnforced => Ok(()),
        // Every right was asked for as a hard requirement, so this does
        // not happen; if it did, the run would not start half confined.
        Ok(_) => Err(std::io::Error::from_raw_os_error(libc::ENOTSUP)),
        Err(e) => {
            let mut source: Option<&dyn std::error::Error> = Some(&e);
            while let Some(error) = source {
                if let Some(code) = error
                    .downcast_ref::<std::io::Error>()
                    .and_then(std::io::Error::raw_os_error)
                {
                    return Err(std::io::Error::from_raw_os_error(code));
                }
                source = error.source();
            }
            Err(std::io::Error::from_raw_os_error(libc::EPERM))
        }
    }
}

/// The child loads the seccomp filters. Through prctl(2), not seccomp(2),
/// which systemd's @system-service does not allow. After the fork: see
/// [`Tool::command`].
fn install(filters: &Filters) -> std::io::Result<()> {
    // SAFETY: prctl(2) with integer arguments, and with a pointer to a
    // filter that lives as long as `filters` (the closure's).
    unsafe {
        if libc::prctl(
            libc::PR_SET_NO_NEW_PRIVS,
            1 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        for program in &filters.0 {
            let fprog = libc::sock_fprog {
                // At most BPF_MAXINSNS, which `filters` checked.
                len: program.len() as libc::c_ushort,
                filter: program.as_ptr().cast::<libc::sock_filter>().cast_mut(),
            };
            if libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                &raw const fprog,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// What a run's environment keeps from the server's: where programs and
/// their data are, and the GPU drivers' settings. Nothing else: the
/// server's environment may name its credentials, or hold a token.
const KEPT: &[&str] = &[
    "PATH",
    "LD_LIBRARY_PATH",
    "LOCALE_ARCHIVE",
    "TESSDATA_PREFIX",
    "FONTCONFIG_FILE",
    "FONTCONFIG_PATH",
    "LIBVA_DRIVER_NAME",
    "LIBVA_DRIVERS_PATH",
    "LIBVA_MESSAGING_LEVEL",
    "VDPAU_DRIVER",
    "CUDA_VISIBLE_DEVICES",
    "NVIDIA_VISIBLE_DEVICES",
    "__EGL_VENDOR_LIBRARY_DIRS",
];

fn environment(jail: &Jail) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = KEPT
        .iter()
        .filter_map(|key| Some((OsString::from(key), std::env::var_os(key)?)))
        .collect();
    // Caches and temporary files (fontconfig's, say) go where the run
    // may write, or nowhere.
    let home = jail
        .writes
        .first()
        .map_or_else(|| OsString::from("/"), |dir| dir.clone().into_os_string());
    for key in ["HOME", "TMPDIR", "XDG_CACHE_HOME"] {
        env.push((key.into(), home.clone()));
    }
    env
}

/// `program` as a real path: looked up in `PATH` if it is a bare name,
/// then with symlinks resolved, so the rule that lets it run names the
/// file itself.
fn resolve(program: &Path) -> std::io::Result<PathBuf> {
    let found = if program.components().count() == 1 && !program.is_absolute() {
        std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} is not on PATH", program.display()),
                )
            })?
    } else {
        program.to_owned()
    };
    std::fs::canonicalize(found)
}

/// The system's programs and libraries, to read and run in every run.
/// Those missing on this host are left out.
const SYSTEM: &[&str] = &[
    "/nix/store",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    // ffmpeg's subtitles filter (burned-in text) finds fonts through
    // fontconfig.
    "/etc/fonts",
];

/// The device nodes every run may use: the ones written to, then those
/// only read.
const DEVICES_WRITTEN: &[&str] = &["/dev/null", "/dev/full"];
const DEVICES_READ: &[&str] = &["/dev/zero", "/dev/random", "/dev/urandom"];

/// What the GPU drivers read, with [`Jail::gpu`]: `/sys` for the device's
/// PCI ids, and NixOS's driver directory.
const GPU_READS: &[&str] = &["/sys", "/run/opengl-driver"];

/// The Landlock ruleset for `program` in `jail`, ready for the child to
/// confine itself with. Every right this kernel knows is handled, so
/// anything not allowed here is refused.
fn ruleset(
    abi: i32,
    sandbox: &Inner,
    program: Option<&Path>,
    jail: &Jail,
) -> std::io::Result<RulesetCreated> {
    let abi = ABI::from(abi);
    let all = AccessFs::from_all(abi);
    let net = AccessNet::from_all(abi);
    let scope = Scope::from_all(abi);
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(all)
        .map_err(std::io::Error::other)?;
    // No TCP connect or bind at all (ABI 4): no port is allowed.
    if !net.is_empty() {
        ruleset = ruleset.handle_access(net).map_err(std::io::Error::other)?;
    }
    // No signal or abstract Unix socket out of the run (ABI 6).
    if !scope.is_empty() {
        ruleset = ruleset.scope(scope).map_err(std::io::Error::other)?;
    }
    let mut rules = Rules {
        abi,
        all,
        ruleset: ruleset.create().map_err(std::io::Error::other)?,
    };
    let run = AccessFs::from_read(abi);
    let read = AccessFs::ReadFile | AccessFs::ReadDir;
    for dir in SYSTEM {
        rules.allow(Path::new(dir), run, false)?;
    }
    if let Some(program) = program
        && !SYSTEM.iter().any(|dir| program.starts_with(dir))
    {
        rules.allow(program, run, true)?;
    }
    for device in DEVICES_WRITTEN {
        let access = AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate;
        rules.allow(Path::new(device), access, false)?;
    }
    for device in DEVICES_READ {
        rules.allow(Path::new(device), AccessFs::ReadFile.into(), false)?;
    }
    if jail.gpu {
        for device in &sandbox.gpu {
            let access = AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::IoctlDev;
            rules.allow(device, access, false)?;
            // libdrm finds the render node by listing /dev/dri.
            if let Some(parent) = device.parent()
                && parent != Path::new("/dev")
            {
                rules.allow(parent, AccessFs::ReadDir.into(), false)?;
            }
        }
        for dir in GPU_READS {
            rules.allow(Path::new(dir), run, false)?;
        }
    }
    for dir in &jail.reads {
        rules.allow(dir, read, true)?;
    }
    for dir in &jail.writes {
        // Everything but running what it wrote.
        rules.allow(dir, all & !AccessFs::Execute, true)?;
    }
    for input in &jail.inputs {
        if let Some(file) = open_input(input, sandbox, jail)? {
            rules.allow_file(file, AccessFs::ReadFile.into())?;
        }
    }
    Ok(rules.ruleset)
}

/// A ruleset being filled.
struct Rules {
    abi: ABI,
    all: BitFlags<AccessFs>,
    ruleset: RulesetCreated,
}

impl Rules {
    /// `access` beneath `path`; a `path` that does not exist is an error
    /// only when it `must`.
    fn allow(
        &mut self,
        path: &Path,
        access: BitFlags<AccessFs>,
        must: bool,
    ) -> std::io::Result<()> {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if !must && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("{}: {e}", path.display()),
                ));
            }
        };
        self.allow_file(file, access)
    }

    /// `access` beneath what `file` is open on: that inode, whatever path
    /// leads to it later. A file that is not a directory gets only the
    /// rights a file can have.
    fn allow_file(&mut self, file: File, access: BitFlags<AccessFs>) -> std::io::Result<()> {
        let mut access = access & self.all;
        if !file.metadata()?.is_dir() {
            access &= AccessFs::from_file(self.abi);
        }
        if access.is_empty() {
            return Ok(());
        }
        (&mut self.ruleset)
            .add_rule(PathBeneath::new(file, access))
            .map_err(std::io::Error::other)?;
        Ok(())
    }
}

/// `path`, opened to be read in a run; `None` if it does not exist.
fn open_input(path: &Path, sandbox: &Inner, jail: &Jail) -> std::io::Result<Option<File>> {
    let refused = |why: String| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{}: {why}", path.display()),
        )
    };
    // Non-blocking: a FIFO in a library folder must not hang the open.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(refused("not a regular file".into()));
    }
    // Where the open ended up, symlinks followed: what the run would read.
    let real = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .map_err(|e| refused(format!("cannot tell where it leads: {e}")))?;
    let own = jail
        .reads
        .iter()
        .chain(&jail.writes)
        .filter_map(|dir| std::fs::canonicalize(dir).ok())
        .any(|dir| real.starts_with(dir));
    if !own && sandbox.hidden.iter().any(|dir| real.starts_with(dir)) {
        return Err(refused(format!(
            "leads to {}, which a media tool may not read",
            real.display()
        )));
    }
    Ok(Some(file))
}

/// The most instructions a seccomp filter may have (BPF_MAXINSNS).
const BPF_MAXINSNS: usize = 4096;

/// The seccomp filters: the calls refused with `EPERM`, then `clone3`
/// refused with `ENOSYS`, so that the C library starts threads with
/// `clone`, whose flags the first filter can see.
fn filters() -> Result<Filters, String> {
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| e.to_string())?;
    let refused = SeccompFilter::new(
        refused_calls().map_err(|e| e.to_string())?,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|e| e.to_string())?;
    let clone3 = SeccompFilter::new(
        BTreeMap::from([(libc::SYS_clone3, Vec::new())]),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        arch,
    )
    .map_err(|e| e.to_string())?;
    let programs = [refused, clone3]
        .into_iter()
        .map(|filter| {
            let program: BpfProgram = filter
                .try_into()
                .map_err(|e: seccompiler::BackendError| e.to_string())?;
            if program.is_empty() || program.len() > BPF_MAXINSNS {
                return Err(format!(
                    "a seccomp filter of {} instructions",
                    program.len()
                ));
            }
            Ok(program)
        })
        .collect::<Result<_, String>>()?;
    Ok(Filters(programs))
}

/// The calls the first filter refuses: each always, or when its rule
/// holds.
fn refused_calls() -> Result<BTreeMap<i64, Vec<SeccompRule>>, seccompiler::BackendError> {
    let mut calls: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for call in [
        // Another process's memory, or the kernel's.
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_pidfd_getfd,
        libc::SYS_kcmp,
        libc::SYS_perf_event_open,
        libc::SYS_bpf,
        libc::SYS_userfaultfd,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        // Mounts and namespaces: a user namespace would give the run
        // rights over a view of its own.
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_open_tree,
        libc::SYS_move_mount,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        libc::SYS_mount_setattr,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        // Keyrings, modules, the clock, the machine.
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_acct,
        libc::SYS_quotactl,
        libc::SYS_syslog,
        libc::SYS_settimeofday,
        libc::SYS_clock_settime,
        libc::SYS_clock_adjtime,
        libc::SYS_adjtimex,
        libc::SYS_sethostname,
        libc::SYS_setdomainname,
        libc::SYS_vhangup,
        libc::SYS_fanotify_init,
        // Connecting to anything: with no mount namespace the host's Unix
        // sockets (D-Bus, the journal) are in plain sight, and Landlock
        // leaves a pathname socket open before ABI 9. These programs make
        // no connection; a socketpair needs none.
        libc::SYS_connect,
    ] {
        calls.insert(call, Vec::new());
    }
    // New processes: these programs only start threads.
    #[cfg(target_arch = "x86_64")]
    for call in [
        libc::SYS_fork,
        libc::SYS_vfork,
        libc::SYS_iopl,
        libc::SYS_ioperm,
        libc::SYS_uselib,
    ] {
        calls.insert(call, Vec::new());
    }
    calls.insert(
        libc::SYS_clone,
        vec![SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Qword,
            SeccompCmpOp::MaskedEq(libc::CLONE_THREAD as u64),
            0,
        )?])?],
    );
    // No network but Unix sockets.
    calls.insert(
        libc::SYS_socket,
        vec![SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
        )?])?],
    );
    // Nor a datagram sent to a named socket without connecting: send(2)
    // gives sendto(2) no address.
    calls.insert(
        libc::SYS_sendto,
        vec![SeccompRule::new(vec![SeccompCondition::new(
            4,
            SeccompCmpArgLen::Qword,
            SeccompCmpOp::Ne,
            0,
        )?])?],
    );
    // Typing into the terminal the server was started from.
    calls.insert(
        libc::SYS_ioctl,
        [libc::TIOCSTI, libc::TIOCLINUX]
            .into_iter()
            .map(|request| {
                #[allow(
                    clippy::unnecessary_cast,
                    reason = "an ioctl request is a u64 here, an i32 in other C libraries"
                )]
                let request = request as u64;
                SeccompRule::new(vec![SeccompCondition::new(
                    1,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Eq,
                    request,
                )?])
            })
            .collect::<Result<_, _>>()?,
    );
    Ok(calls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filters_compile_for_this_machine() {
        let filters = filters().unwrap();
        assert_eq!(filters.0.len(), 2);
        assert!(filters.0.iter().all(|f| !f.is_empty()));
    }

    #[test]
    fn a_mode_is_one_of_three_words() {
        assert_eq!("required".parse(), Ok(Mode::Required));
        assert_eq!("auto".parse(), Ok(Mode::Auto));
        assert_eq!("off".parse(), Ok(Mode::Off));
        assert!("on".parse::<Mode>().is_err());
    }

    fn with(mode: Mode, abi: Result<i32, String>) -> Result<Sandbox, SandboxError> {
        Sandbox::with_landlock(mode, abi, Vec::new(), Vec::new())
    }

    #[test]
    fn required_refuses_a_kernel_without_landlock() {
        let err = with(Mode::Required, Err("this kernel has no Landlock".into())).unwrap_err();
        assert!(err.to_string().contains("no Landlock"), "{err}");
    }

    #[test]
    fn required_refuses_a_landlock_that_cannot_refuse_truncation() {
        let err = with(Mode::Required, Ok(2)).unwrap_err();
        assert!(err.to_string().contains("ABI 2"), "{err}");
    }

    #[test]
    fn auto_without_landlock_runs_with_seccomp_alone() {
        let sandbox = with(Mode::Auto, Err("this kernel has no Landlock".into())).unwrap();
        assert!(!sandbox.confines());
        assert!(sandbox.0.seccomp.is_some());
    }

    #[test]
    fn off_confines_nothing() {
        let sandbox = with(Mode::Off, landlock_abi()).unwrap();
        assert!(!sandbox.confines());
        assert!(sandbox.0.seccomp.is_none());
    }

    #[test]
    fn this_kernel_confines() {
        let sandbox = with(Mode::Required, landlock_abi()).unwrap();
        assert!(sandbox.landlock_abi().unwrap() >= MIN_LANDLOCK_ABI);
        assert!(sandbox.0.seccomp.is_some());
    }
}
