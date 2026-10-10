//! The server's own process, hardened (PX-31). The media tools are
//! confined ([`crate::sandbox`]); this is what the server process itself
//! does to itself, the same in every deployment, before it starts a
//! thread:
//!
//! - **Not dumpable** (`PR_SET_DUMPABLE` 0): its `/proc/<pid>/*` is owned
//!   by root, so a process as the same user can no longer read its
//!   `environ` (with the `SF_ENV_` secrets), `mem`, `maps` or `fd`, nor
//!   attach a debugger. Root still can. The process still reads what it
//!   uses of its own `/proc/self` (`statm`, `status`, `maps`, `fd`,
//!   `exe`); its entries only their owner may open (`environ`, `mem`,
//!   `auxv`) become root's, and it reads none of them. Exec makes a
//!   program dumpable again, so the
//!   media tools are not affected; between fork and exec their child is
//!   not dumpable, and opens its own `/proc/self` entries as itself.
//! - **No new privileges** (`PR_SET_NO_NEW_PRIVS` 1): a setuid or
//!   file-capability program run by the server gains nothing. The NVIDIA
//!   driver runs the setuid `nvidia-modprobe` when its device nodes are
//!   missing: it then cannot make them. The media tools always ran so.
//! - **No core dump** (`RLIMIT_CORE` 0, soft and hard): no keys or
//!   session secrets on disk after a crash, and no child can raise it.
//! - **A seccomp deny-list**: the calls the server never makes, refused
//!   with `EPERM` (not a kill: a call it turns out to need is an error in
//!   the log, not a dead server), and `clone3` refused with `ENOSYS`, so
//!   the C library starts threads and processes with `clone`, whose flags
//!   the filter sees. It is not an allowlist like the tools': the server
//!   runs programs, and uses every socket family tokio, iroh, rustls and
//!   SQLite need. The tools inherit it, and their own filter stacks on
//!   top; nothing a tool's child calls to confine itself (`setrlimit`,
//!   `prctl`, Landlock's calls, `open`, `close`) is in it.
//!
//! Loaded with prctl(2), as the tools' filter is: systemd's
//! `@system-service` does not allow seccomp(2).

use std::collections::BTreeMap;

use seccompiler::{
    SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    TargetArch,
};

use crate::sandbox::{Filters, compile, install};

/// What [`harden`] did with the seccomp deny-list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// Loaded now.
    Loaded,
    /// Kept: the process restarted in place ([`keep_across_exec`]) and
    /// carries it from before. Loaded again, it would stack a copy at
    /// every restart, up to the kernel's limit.
    Kept,
    /// None for this machine, and why: the rest still applies.
    Unavailable(String),
}

#[derive(Debug, thiserror::Error)]
pub enum HardenError {
    #[error("cannot {what}: {source}")]
    Call {
        what: &'static str,
        source: std::io::Error,
    },
}

/// Set by [`keep_across_exec`] to the process's id: the process that
/// starts with it, as that id, already has the deny-list.
const KEPT: &str = "SPOOLFRAME_HARDENED_PID";

/// Harden this process (see the module's doc). The flags and the limit
/// are the process's; the filter is the calling thread's and every
/// thread and process it starts after, so call it before any other
/// thread exists. A call that fails is an error; a machine with no
/// filter is [`Filter::Unavailable`].
pub fn harden() -> Result<Filter, HardenError> {
    // SAFETY: prctl(2) with integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(failed("make the process not dumpable"));
    }
    let none = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: setrlimit(2) with a valid pointer.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &none) } != 0 {
        return Err(failed("turn core dumps off"));
    }
    if restarted() {
        return Ok(Filter::Kept);
    }
    match deny_list(libc::EPERM) {
        Ok(filters) => {
            install(&filters).map_err(|source| HardenError::Call {
                what: "load the seccomp deny-list",
                source,
            })?;
            Ok(Filter::Loaded)
        }
        Err(reason) => {
            no_new_privileges()?;
            Ok(Filter::Unavailable(reason))
        }
    }
}

/// The deny-list alone, its calls refused with `errno` rather than
/// `EPERM`, and no new privileges: for tests, where `EPERM` would not
/// tell the filter from the kernel's own refusal of an unprivileged call.
#[doc(hidden)]
pub fn refuse_with(errno: i32) -> Result<(), HardenError> {
    let filters = deny_list(errno).map_err(|reason| HardenError::Call {
        what: "make the seccomp deny-list",
        source: std::io::Error::other(reason),
    })?;
    install(&filters).map_err(|source| HardenError::Call {
        what: "load the seccomp deny-list",
        source,
    })
}

/// Before this process runs a program in its own place (a restart in
/// place): the program is this one, and keeps the deny-list, which exec
/// does not take away.
pub fn keep_across_exec(command: &mut std::process::Command) {
    command.env(KEPT, std::process::id().to_string());
}

/// Whether this process restarted in place with the deny-list loaded:
/// [`keep_across_exec`] named it, and it has a seccomp filter and no new
/// privileges. The variable is taken out of the environment either way.
fn restarted() -> bool {
    let Some(named) = std::env::var_os(KEPT) else {
        return false;
    };
    // SAFETY: before any other thread exists (see `harden`).
    unsafe { std::env::remove_var(KEPT) };
    // SAFETY: prctl(2) PR_GET_ options only read.
    let (seccomp, no_new_privs) = unsafe {
        (
            libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0),
            libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0),
        )
    };
    named == std::process::id().to_string().as_str()
        && seccomp == libc::SECCOMP_MODE_FILTER as libc::c_int
        && no_new_privs == 1
}

fn no_new_privileges() -> Result<(), HardenError> {
    // SAFETY: prctl(2) with integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(failed("set no new privileges"));
    }
    Ok(())
}

fn failed(what: &'static str) -> HardenError {
    HardenError::Call {
        what,
        source: std::io::Error::last_os_error(),
    }
}

/// The deny-list's filters: its calls refused with `errno`, then `clone3`
/// with `ENOSYS`.
fn deny_list(errno: i32) -> Result<Filters, String> {
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| e.to_string())?;
    let errno = u32::try_from(errno).map_err(|e| e.to_string())?;
    let refused = SeccompFilter::new(
        refused_calls().map_err(|e| e.to_string())?,
        SeccompAction::Allow,
        SeccompAction::Errno(errno),
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
    compile([refused, clone3])
}

/// The namespaces a clone may not make. `CLONE_NEWTIME` is in clone(2)'s
/// exit-signal bits: only unshare(2) and clone3(2) take it, and both are
/// refused.
const NEW_NAMESPACES: [libc::c_int; 7] = [
    libc::CLONE_NEWNS,
    libc::CLONE_NEWCGROUP,
    libc::CLONE_NEWUTS,
    libc::CLONE_NEWIPC,
    libc::CLONE_NEWUSER,
    libc::CLONE_NEWPID,
    libc::CLONE_NEWNET,
];

/// `personality(2)`'s query, which changes nothing.
const PERSONA_QUERY: u64 = 0xffff_ffff;

/// The calls the deny-list refuses: each always, or when its rule holds.
fn refused_calls() -> Result<BTreeMap<i64, Vec<SeccompRule>>, seccompiler::BackendError> {
    let mut calls: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for call in [
        // Another process's memory or descriptors, or the kernel's.
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_pidfd_getfd,
        libc::SYS_kcmp,
        libc::SYS_perf_event_open,
        libc::SYS_bpf,
        libc::SYS_userfaultfd,
        libc::SYS_lookup_dcookie,
        // tokio does not use io_uring.
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        // Mounts and namespaces.
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
        // Keyrings, modules, the machine.
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
    ] {
        calls.insert(call, Vec::new());
    }
    // I/O ports. (vm86 is 32-bit x86's alone.)
    #[cfg(target_arch = "x86_64")]
    for call in [libc::SYS_iopl, libc::SYS_ioperm] {
        calls.insert(call, Vec::new());
    }
    // A clone into a new namespace; threads and processes are let through.
    calls.insert(
        libc::SYS_clone,
        NEW_NAMESPACES
            .into_iter()
            .map(|flag| {
                let flag = flag as u64;
                SeccompRule::new(vec![SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Qword,
                    SeccompCmpOp::MaskedEq(flag),
                    flag,
                )?])
            })
            .collect::<Result<_, _>>()?,
    );
    // Setting any persona but Linux's (0); the query sets nothing.
    calls.insert(
        libc::SYS_personality,
        vec![SeccompRule::new(vec![
            SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, 0)?,
            SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, PERSONA_QUERY)?,
        ])?],
    );
    Ok(calls)
}
