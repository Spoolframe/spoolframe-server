//! ffmpeg, ffprobe and tesseract confined (PX-11 finding 34, PX-09 B-12).
//!
//! They parse files anyone who can write to a library folder made, in C.
//! So each run gets only what it needs:
//!
//! - **bubblewrap** (`bwrap`), when [`Mode`] lets it confine: new user,
//!   mount, PID, network, IPC and UTS namespaces. The root is an empty
//!   read-only tmpfs; the system's programs and libraries (the nix store,
//!   `/usr`, …) are bound in read-only. Each input file is opened here and
//!   bound in by that file descriptor, so what the run reads is the file
//!   that was checked, not one swapped in after. The job's own directory
//!   is the one writable path. No network, no `/proc`, no state directory,
//!   no `/tmp`, no `/home`. A seccomp filter refuses new processes
//!   (threads only), sockets but Unix ones, and the calls a sandbox escape
//!   uses: `ptrace`, `mount`, namespaces, keyrings, `bpf`, `io_uring`, …
//! - **always**, confined or not: a clean environment (a short allowlist,
//!   and `HOME` and `TMPDIR` in the job's directory), resource limits
//!   (memory, CPU time, file size, no core dumps), and a process group of
//!   its own, so a signal reaches the whole run.
//!
//! bubblewrap is a program, not a library, so nothing here forks with a
//! half-set-up namespace in a multithreaded server: it is the same
//! sandbox Flatpak runs on, and nixpkgs has it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use seccompiler::{
    SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    TargetArch,
};

/// `media.sandbox`: whether the tools must be confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// bubblewrap, or the server does not start.
    Required,
    /// bubblewrap if it works here; otherwise unconfined, said loudly.
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
    #[error("media.sandbox is \"required\", but {bwrap} cannot confine a program here: {reason}")]
    Unavailable { bwrap: String, reason: String },
}

/// How the tools run: confined by bubblewrap, or not. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Sandbox(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    bwrap: Option<Bwrap>,
    /// No input is read from in here: the state directory, with the
    /// database, the key and the secrets; systemd's credentials.
    hidden: Vec<PathBuf>,
    /// The GPU's device nodes, for a run that asks for them.
    gpu: Vec<PathBuf>,
}

#[derive(Debug)]
struct Bwrap {
    /// bwrap itself, its real path.
    path: PathBuf,
    /// The seccomp filters, as BPF, in the order they are loaded.
    filters: Vec<Vec<u8>>,
}

impl Sandbox {
    /// Nothing confined: for tests with stand-in programs, and
    /// `media.sandbox = "off"`.
    pub fn off() -> Self {
        Self(Arc::new(Inner {
            bwrap: None,
            hidden: Vec::new(),
            gpu: Vec::new(),
        }))
    }

    /// Confine with `bwrap` (a path, or a name looked up in `PATH`) as
    /// `mode` says. bwrap is tried once here, the way every run uses it;
    /// if that fails, `Required` is an error and `Auto` runs unconfined
    /// and logs why. No input of a run may be inside `hidden`, and a run
    /// that uses the GPU gets the device nodes in `gpu`.
    pub fn new(
        mode: Mode,
        bwrap: &Path,
        hidden: Vec<PathBuf>,
        gpu: Vec<PathBuf>,
    ) -> Result<Self, SandboxError> {
        let hidden = hidden
            .into_iter()
            .map(|dir| std::fs::canonicalize(&dir).unwrap_or(dir))
            .collect();
        let bwrap = match mode {
            Mode::Off => {
                tracing::warn!(
                    "media.sandbox is \"off\": ffmpeg, ffprobe and tesseract run with the server's \
                     own access to files and the network"
                );
                None
            }
            Mode::Required | Mode::Auto => match Bwrap::check(bwrap) {
                Ok(bwrap) => {
                    tracing::info!(bwrap = %bwrap.path.display(), "media tools run confined");
                    Some(bwrap)
                }
                Err(reason) if mode == Mode::Required => {
                    return Err(SandboxError::Unavailable {
                        bwrap: bwrap.display().to_string(),
                        reason,
                    });
                }
                Err(reason) => {
                    tracing::error!(
                        bwrap = %bwrap.display(),
                        reason,
                        "MEDIA TOOLS RUN UNCONFINED: bubblewrap cannot confine them here, so \
                         ffmpeg, ffprobe and tesseract parse untrusted files with the server's own \
                         access to its state directory and the network. Install bubblewrap, or set \
                         media.sandbox = \"required\" to refuse to start without it"
                    );
                    None
                }
            },
        };
        Ok(Self(Arc::new(Inner { bwrap, hidden, gpu })))
    }

    /// Whether runs are confined.
    pub fn confines(&self) -> bool {
        self.0.bwrap.is_some()
    }
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
    /// a subtitle beside it. Opened by the server and bound in by its file
    /// descriptor, following symlinks; refused unless a regular file
    /// outside the hidden directories. One that does not exist is left
    /// out, and the program says so as it would anyway.
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
        let mut keep: Vec<OwnedFd> = Vec::new();
        let mut command = match &self.sandbox.0.bwrap {
            None => std::process::Command::new(&self.program),
            Some(bwrap) => {
                let program = resolve(&self.program)?;
                let mut command = std::process::Command::new(&bwrap.path);
                let args = bwrap.args(&self.sandbox.0, &program, jail, &mut keep)?;
                command.args(args).arg("--").arg(&program);
                command
            }
        };
        command.env_clear();
        for (key, value) in environment(jail) {
            command.env(key, value);
        }
        command.process_group(0);
        let fds: Vec<RawFd> = keep.iter().map(AsRawFd::as_raw_fd).collect();
        let limits = jail.limits;
        let memory = limits.memory.bytes();
        // SAFETY: between fork and exec only fcntl(2), getrlimit(2) and
        // setrlimit(2) run, which are async-signal-safe; nothing allocates
        // (`fds` was made before the fork) or takes a lock.
        unsafe {
            command.pre_exec(move || {
                for &fd in &fds {
                    // bwrap reads these by number: they must survive exec.
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                lower(libc::RLIMIT_DATA, memory)?;
                lower(libc::RLIMIT_CPU, limits.cpu_secs)?;
                lower(libc::RLIMIT_FSIZE, limits.file_size)?;
                lower(libc::RLIMIT_CORE, 0)?;
                Ok(())
            });
        }
        Ok(Command {
            command: tokio::process::Command::from(command),
            _keep: keep,
        })
    }
}

/// A command from [`Tool::command`]. It holds the inputs' file
/// descriptors until it is dropped, so it must outlive its spawn.
#[derive(Debug)]
pub struct Command {
    command: tokio::process::Command,
    _keep: Vec<OwnedFd>,
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
/// then with symlinks resolved, since `/run/current-system` and the like
/// are not in the sandbox.
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

/// The system's programs and libraries, read-only in every run. Those
/// missing on this host are left out.
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

/// What the GPU drivers read, with [`Jail::gpu`]: `/sys` for the device's
/// PCI ids, and NixOS's driver directory.
const GPU_READS: &[&str] = &["/sys", "/run/opengl-driver"];

impl Bwrap {
    /// bwrap at `path`, if it confines a program here: it runs itself,
    /// confined, as a run would.
    fn check(path: &Path) -> Result<Self, String> {
        let real = resolve(path).map_err(|e| format!("cannot find it: {e}"))?;
        let bwrap = Self {
            path: real.clone(),
            filters: filters()?,
        };
        let mut keep = Vec::new();
        let args = bwrap
            .args(
                &Inner {
                    bwrap: None,
                    hidden: Vec::new(),
                    gpu: Vec::new(),
                },
                &real,
                &Jail::new(Limits::PROBE),
                &mut keep,
            )
            .map_err(|e| e.to_string())?;
        let fds: Vec<RawFd> = keep.iter().map(AsRawFd::as_raw_fd).collect();
        let mut command = std::process::Command::new(&real);
        command
            .args(args)
            .arg("--")
            .arg(&real)
            .arg("--version")
            .env_clear()
            .stdin(std::process::Stdio::null());
        // SAFETY: as in `Tool::command`: only fcntl(2) after the fork.
        unsafe {
            command.pre_exec(move || {
                for &fd in &fds {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let output = command
            .output()
            .map_err(|e| format!("cannot run it: {e}"))?;
        drop(keep);
        if output.status.success() {
            Ok(bwrap)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "{} ({})",
                stderr.trim().lines().last().unwrap_or("no message"),
                output.status
            ))
        }
    }

    /// bwrap's arguments for `program` in `jail`, up to the `--`. The file
    /// descriptors they name go in `keep`.
    fn args(
        &self,
        sandbox: &Inner,
        program: &Path,
        jail: &Jail,
        keep: &mut Vec<OwnedFd>,
    ) -> std::io::Result<Vec<OsString>> {
        let mut args: Vec<OsString> = [
            "--unshare-user",
            "--unshare-ipc",
            "--unshare-pid",
            "--unshare-net",
            "--unshare-uts",
            "--die-with-parent",
            // The program is the namespace's first process: a signal to
            // the run's process group reaches it, and its exit status is
            // bwrap's.
            "--as-pid-1",
        ]
        .map(OsString::from)
        .to_vec();
        let mut bind = |flag: &str, from: &Path, to: &Path| {
            args.push(flag.into());
            args.push(from.into());
            args.push(to.into());
        };
        for dir in SYSTEM {
            bind("--ro-bind-try", Path::new(dir), Path::new(dir));
        }
        if !SYSTEM.iter().any(|dir| program.starts_with(dir)) {
            bind("--ro-bind", program, program);
        }
        args.extend(["--dev", "/dev"].map(OsString::from));
        if jail.gpu {
            for device in &sandbox.gpu {
                args.push("--dev-bind-try".into());
                args.push(device.into());
                args.push(device.into());
            }
            for dir in GPU_READS {
                args.push("--ro-bind-try".into());
                args.push(dir.into());
                args.push(dir.into());
            }
        }
        for dir in &jail.reads {
            let dir = std::path::absolute(dir)?;
            args.extend(["--ro-bind".into(), dir.clone().into(), dir.into()]);
        }
        for dir in &jail.writes {
            let dir = std::path::absolute(dir)?;
            args.extend(["--bind".into(), dir.clone().into(), dir.into()]);
        }
        for input in &jail.inputs {
            let Some(file) = open_input(input, sandbox, jail)? else {
                continue;
            };
            args.push("--ro-bind-fd".into());
            args.push(file.as_raw_fd().to_string().into());
            args.push(std::path::absolute(input)?.into());
            keep.push(file);
        }
        for filter in &self.filters {
            let (reader, mut writer) = std::io::pipe()?;
            writer.write_all(filter)?;
            drop(writer);
            let fd = OwnedFd::from(reader);
            args.push("--add-seccomp-fd".into());
            args.push(fd.as_raw_fd().to_string().into());
            keep.push(fd);
        }
        args.extend(["--chdir", "/", "--remount-ro", "/"].map(OsString::from));
        Ok(args)
    }
}

/// `path`, opened to be bound into a run; `None` if it does not exist.
fn open_input(path: &Path, sandbox: &Inner, jail: &Jail) -> std::io::Result<Option<OwnedFd>> {
    let refused = |why: String| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{}: {why}", path.display()),
        )
    };
    // Non-blocking: a FIFO in a library folder must not hang the open.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
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
    Ok(Some(OwnedFd::from(file)))
}

/// The seccomp filters, as BPF for bwrap's `--add-seccomp-fd`: the calls
/// refused with `EPERM`, then `clone3` refused with `ENOSYS`, so that the
/// C library starts threads with `clone`, whose flags the first filter
/// can see.
fn filters() -> Result<Vec<Vec<u8>>, String> {
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
    [refused, clone3]
        .into_iter()
        .map(|filter| {
            let program: seccompiler::BpfProgram = filter
                .try_into()
                .map_err(|e: seccompiler::BackendError| e.to_string())?;
            Ok(program
                .iter()
                .flat_map(|op| {
                    let mut bytes = Vec::with_capacity(8);
                    bytes.extend(op.code.to_ne_bytes());
                    bytes.push(op.jt);
                    bytes.push(op.jf);
                    bytes.extend(op.k.to_ne_bytes());
                    bytes
                })
                .collect())
        })
        .collect()
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
        // Mounts and namespaces: the way out of a bubblewrap sandbox.
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
    // No network but Unix sockets; the namespace has none anyway.
    calls.insert(
        libc::SYS_socket,
        vec![SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
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
        assert_eq!(filters.len(), 2);
        assert!(filters.iter().all(|f| !f.is_empty() && f.len() % 8 == 0));
    }

    #[test]
    fn a_mode_is_one_of_three_words() {
        assert_eq!("required".parse(), Ok(Mode::Required));
        assert_eq!("auto".parse(), Ok(Mode::Auto));
        assert_eq!("off".parse(), Ok(Mode::Off));
        assert!("on".parse::<Mode>().is_err());
    }
}
