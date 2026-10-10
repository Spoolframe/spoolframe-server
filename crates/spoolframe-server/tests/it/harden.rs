//! The server's own process, hardened as `main` hardens it (PX-31). Each
//! test runs again in a process of its own ([`alone`]) and hardens that
//! one: the flags are the process's, and the filter is the hardening
//! thread's and every thread and process it starts after.

use std::process::{Command, Output};

use spoolframe_media::harden;
use spoolframe_media::sandbox::{GpuDevices, Mode, Sandbox, Tool};
use spoolframe_server::metrics::process;

use crate::support::alone;

/// prctl(2) with a `PR_GET_…` option: its answer.
fn get(option: libc::c_int) -> libc::c_int {
    // SAFETY: a PR_GET_ option with no pointer argument only reads.
    unsafe { libc::prctl(option, 0, 0, 0, 0) }
}

/// The soft and hard core-dump limits.
fn core_limit() -> (libc::rlim_t, libc::rlim_t) {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) with a valid pointer.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut limit) }, 0);
    (limit.rlim_cur, limit.rlim_max)
}

/// What a raw system call returned: `Ok` with its value, or `Err` with
/// errno.
fn outcome(returned: libc::c_long) -> Result<libc::c_long, i32> {
    if returned == -1 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap())
    } else {
        Ok(returned)
    }
}

#[test]
fn a_hardened_process_is_not_dumpable_has_no_new_privileges_and_no_core() {
    if !alone("harden::a_hardened_process_is_not_dumpable_has_no_new_privileges_and_no_core") {
        return;
    }
    assert_eq!(get(libc::PR_GET_DUMPABLE), 1, "dumpable before");
    assert_eq!(harden::harden().unwrap(), harden::Filter::Loaded);
    assert_eq!(get(libc::PR_GET_DUMPABLE), 0);
    assert_eq!(get(libc::PR_GET_NO_NEW_PRIVS), 1);
    assert_eq!(core_limit(), (0, 0));
    // The deny-list refuses with EPERM, and clone3 with ENOSYS.
    // SAFETY: each call below fails, or does nothing.
    unsafe {
        assert_eq!(
            outcome(libc::syscall(libc::SYS_unshare, 0)),
            Err(libc::EPERM)
        );
        assert_eq!(
            outcome(libc::syscall(
                libc::SYS_ptrace,
                libc::PTRACE_PEEKDATA,
                1,
                0,
                0
            )),
            Err(libc::EPERM)
        );
        assert_eq!(
            outcome(libc::syscall(libc::SYS_clone3, 0, 0)),
            Err(libc::ENOSYS)
        );
    }
}

/// `cat` of `path`, run by this process: another process as the same
/// user.
fn cat(path: &str) -> Output {
    Command::new("cat").arg(path).output().unwrap()
}

#[test]
fn a_process_as_the_same_user_cannot_read_a_hardened_process_s_environ() {
    if !alone("harden::a_process_as_the_same_user_cannot_read_a_hardened_process_s_environ") {
        return;
    }
    let environ = format!("/proc/{}/environ", std::process::id());
    let before = cat(&environ);
    assert!(before.status.success(), "{before:?}");
    assert!(
        String::from_utf8_lossy(&before.stdout).contains("SPOOLFRAME_TEST_ALONE=1"),
        "{before:?}"
    );

    harden::harden().unwrap();
    let after = cat(&environ);
    assert!(!after.status.success(), "{after:?}");
    assert!(after.stdout.is_empty(), "{after:?}");
    assert!(
        String::from_utf8_lossy(&after.stderr).contains("Permission denied"),
        "{after:?}"
    );
    let mem = cat(&format!("/proc/{}/mem", std::process::id()));
    assert!(
        String::from_utf8_lossy(&mem.stderr).contains("Permission denied"),
        "{mem:?}"
    );

    // Its own, the process still reads what it uses: what the metrics
    // read, its status and maps, where a descriptor leads (the media
    // sandbox's check of an input), and its program (a restart in
    // place). Its entries only root may open (`environ`, `mem`) are
    // root's now, not its own.
    assert!(process::resident_bytes().is_some());
    assert!(process::open_files().is_some());
    std::fs::read_to_string("/proc/self/status").unwrap();
    std::fs::read_to_string("/proc/self/maps").unwrap();
    let file = std::fs::File::open("/proc/self/status").unwrap();
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    assert!(std::fs::read_link(format!("/proc/self/fd/{fd}")).is_ok());
    assert!(std::env::current_exe().is_ok());
    let own = std::fs::read("/proc/self/environ");
    assert_eq!(
        own.map_err(|e| e.kind()).err(),
        Some(std::io::ErrorKind::PermissionDenied)
    );
}

/// The errno [`harden::refuse_with`] is given here: none of the calls
/// below fails with it on its own.
const MARK: i32 = libc::ENOTRECOVERABLE;

/// A call the deny-list refuses: its name, and the call.
type Call = (&'static str, Box<dyn Fn() -> libc::c_long>);

/// Each call the deny-list refuses, with arguments that make it fail, or
/// do nothing, when it is let through.
fn refused() -> Vec<Call> {
    let null = std::ptr::null::<libc::c_void>();
    let me = libc::c_long::from(std::process::id() as i32);
    let missing = c"/nonexistent-px31".as_ptr();
    // SAFETY (each closure): the arguments are null, invalid or
    // harmless, so each call fails or does nothing.
    let mut calls: Vec<Call> = vec![
        (
            "ptrace",
            Box::new(move || unsafe {
                libc::syscall(libc::SYS_ptrace, libc::PTRACE_PEEKDATA, 1, 0, 0)
            }),
        ),
        (
            "process_vm_readv",
            Box::new(move || unsafe {
                libc::syscall(libc::SYS_process_vm_readv, me, null, 0, null, 0, 0)
            }),
        ),
        (
            "process_vm_writev",
            Box::new(move || unsafe {
                libc::syscall(libc::SYS_process_vm_writev, me, null, 0, null, 0, 0)
            }),
        ),
        (
            "pidfd_getfd",
            Box::new(|| unsafe { libc::syscall(libc::SYS_pidfd_getfd, -1, 0, 0) }),
        ),
        (
            "kcmp",
            Box::new(move || unsafe { libc::syscall(libc::SYS_kcmp, me, me, 99, 0, 0) }),
        ),
        (
            "perf_event_open",
            Box::new(move || unsafe {
                libc::syscall(libc::SYS_perf_event_open, null, 0, -1, -1, 0)
            }),
        ),
        (
            "bpf",
            Box::new(move || unsafe { libc::syscall(libc::SYS_bpf, 9999, null, 0) }),
        ),
        // UFFD_USER_MODE_ONLY, which needs no privilege, and a bit that
        // is no flag.
        (
            "userfaultfd",
            Box::new(|| unsafe { libc::syscall(libc::SYS_userfaultfd, 1 | 0x4000) }),
        ),
        (
            "io_uring_setup",
            Box::new(move || unsafe { libc::syscall(libc::SYS_io_uring_setup, 0, null) }),
        ),
        (
            "io_uring_enter",
            Box::new(move || unsafe {
                libc::syscall(libc::SYS_io_uring_enter, -1, 0, 0, 0, null, 0)
            }),
        ),
        (
            "io_uring_register",
            Box::new(move || unsafe { libc::syscall(libc::SYS_io_uring_register, -1, 0, null, 0) }),
        ),
        (
            "mount",
            Box::new(move || unsafe {
                libc::syscall(libc::SYS_mount, null, missing, null, 0, null)
            }),
        ),
        (
            "umount2",
            Box::new(move || unsafe { libc::syscall(libc::SYS_umount2, missing, 0) }),
        ),
        (
            "pivot_root",
            Box::new(move || unsafe { libc::syscall(libc::SYS_pivot_root, missing, missing) }),
        ),
        (
            "chroot",
            Box::new(move || unsafe { libc::syscall(libc::SYS_chroot, missing) }),
        ),
        (
            "unshare",
            Box::new(|| unsafe { libc::syscall(libc::SYS_unshare, 0) }),
        ),
        (
            "setns",
            Box::new(|| unsafe { libc::syscall(libc::SYS_setns, -1, 0) }),
        ),
        (
            "open_tree",
            Box::new(move || unsafe {
                libc::syscall(libc::SYS_open_tree, -1, null, 0xffff_ffffu32)
            }),
        ),
        (
            "move_mount",
            Box::new(move || unsafe {
                libc::syscall(libc::SYS_move_mount, -1, null, -1, null, 0xffff_ffffu32)
            }),
        ),
        (
            "fsopen",
            Box::new(move || unsafe { libc::syscall(libc::SYS_fsopen, null, 0) }),
        ),
        (
            "fsconfig",
            Box::new(move || unsafe { libc::syscall(libc::SYS_fsconfig, -1, 0, null, null, 0) }),
        ),
        (
            "fsmount",
            Box::new(|| unsafe { libc::syscall(libc::SYS_fsmount, -1, 0, 0) }),
        ),
        (
            "fspick",
            Box::new(move || unsafe { libc::syscall(libc::SYS_fspick, -1, null, 0) }),
        ),
        (
            "mount_setattr",
            Box::new(move || unsafe {
                libc::syscall(libc::SYS_mount_setattr, -1, null, 0xffff_ffffu32, null, 0)
            }),
        ),
        (
            "open_by_handle_at",
            Box::new(move || unsafe { libc::syscall(libc::SYS_open_by_handle_at, -1, null, 0) }),
        ),
        (
            "keyctl",
            Box::new(|| unsafe { libc::syscall(libc::SYS_keyctl, 9999, 0, 0, 0, 0) }),
        ),
        (
            "add_key",
            Box::new(move || unsafe { libc::syscall(libc::SYS_add_key, null, null, null, 0, 0) }),
        ),
        (
            "request_key",
            Box::new(move || unsafe { libc::syscall(libc::SYS_request_key, null, null, null, 0) }),
        ),
        (
            "init_module",
            Box::new(move || unsafe { libc::syscall(libc::SYS_init_module, null, 0, null) }),
        ),
        (
            "finit_module",
            Box::new(move || unsafe { libc::syscall(libc::SYS_finit_module, -1, null, 0) }),
        ),
        (
            "delete_module",
            Box::new(move || unsafe { libc::syscall(libc::SYS_delete_module, null, 0) }),
        ),
        (
            "kexec_load",
            Box::new(move || unsafe { libc::syscall(libc::SYS_kexec_load, 0, 0, null, 0) }),
        ),
        (
            "kexec_file_load",
            Box::new(move || unsafe {
                libc::syscall(libc::SYS_kexec_file_load, -1, -1, 0, null, 0)
            }),
        ),
        // No magic numbers: refused even with the right to reboot.
        (
            "reboot",
            Box::new(move || unsafe { libc::syscall(libc::SYS_reboot, 0, 0, 0, null) }),
        ),
        (
            "swapon",
            Box::new(move || unsafe { libc::syscall(libc::SYS_swapon, missing, 0) }),
        ),
        (
            "swapoff",
            Box::new(move || unsafe { libc::syscall(libc::SYS_swapoff, missing) }),
        ),
        (
            "acct",
            Box::new(move || unsafe { libc::syscall(libc::SYS_acct, missing) }),
        ),
        (
            "quotactl",
            Box::new(move || unsafe { libc::syscall(libc::SYS_quotactl, 0, missing, 0, null) }),
        ),
        (
            "lookup_dcookie",
            Box::new(move || unsafe { libc::syscall(libc::SYS_lookup_dcookie, 0, null, 0) }),
        ),
        // Another persona than Linux's (PER_LINUX32), set back below.
        (
            "personality",
            Box::new(|| unsafe { libc::syscall(libc::SYS_personality, 0x0008) }),
        ),
    ];
    // A clone into each new namespace, with CLONE_SIGHAND but not
    // CLONE_VM, which the kernel refuses (EINVAL) before it makes one.
    for (name, flag) in [
        ("clone CLONE_NEWNS", libc::CLONE_NEWNS),
        ("clone CLONE_NEWCGROUP", libc::CLONE_NEWCGROUP),
        ("clone CLONE_NEWUTS", libc::CLONE_NEWUTS),
        ("clone CLONE_NEWIPC", libc::CLONE_NEWIPC),
        ("clone CLONE_NEWUSER", libc::CLONE_NEWUSER),
        ("clone CLONE_NEWPID", libc::CLONE_NEWPID),
        ("clone CLONE_NEWNET", libc::CLONE_NEWNET),
    ] {
        let flags = libc::c_long::from(flag | libc::CLONE_SIGHAND);
        calls.push((
            name,
            Box::new(move || unsafe { libc::syscall(libc::SYS_clone, flags, 0, 0, 0, 0) }),
        ));
    }
    #[cfg(target_arch = "x86_64")]
    {
        // Level 0 and no ports: what the process has already.
        calls.push((
            "iopl",
            Box::new(|| unsafe { libc::syscall(libc::SYS_iopl, 0) }),
        ));
        calls.push((
            "ioperm",
            Box::new(|| unsafe { libc::syscall(libc::SYS_ioperm, 0, 0, 0) }),
        ));
    }
    calls
}

/// The persona back to Linux's, after the unfiltered `personality` call.
fn linux_persona() {
    // SAFETY: personality(2) to the default persona.
    unsafe { libc::personality(0) };
}

#[test]
fn the_deny_list_refuses_each_call_the_server_never_makes() {
    if !alone("harden::the_deny_list_refuses_each_call_the_server_never_makes") {
        return;
    }
    let calls = refused();
    for (name, call) in &calls {
        let unfiltered = outcome(call());
        assert_ne!(unfiltered, Err(MARK), "{name} unfiltered");
    }
    linux_persona();
    // SAFETY: clone3(2) with no arguments fails.
    let clone3 = || outcome(unsafe { libc::syscall(libc::SYS_clone3, 0, 0) });
    assert_eq!(clone3(), Err(libc::EINVAL));

    harden::refuse_with(MARK).unwrap();
    let let_through: Vec<String> = calls
        .iter()
        .filter_map(|(name, call)| {
            let refused = outcome(call());
            (refused != Err(MARK)).then(|| format!("{name}: {refused:?}"))
        })
        .collect();
    assert!(let_through.is_empty(), "let through: {let_through:?}");
    // So that the C library starts threads with clone(2), whose flags the
    // filter sees.
    assert_eq!(clone3(), Err(libc::ENOSYS));

    // What the server and the media tools' children use is let through.
    // SAFETY: the calls below query, or fail with EINVAL.
    unsafe {
        assert_eq!(libc::personality(0xffff_ffff), 0, "the persona is queried");
        assert_eq!(libc::personality(0), 0, "the persona is set to Linux's");
        // A clone with no new namespace reaches the kernel.
        let flags = libc::c_long::from(libc::CLONE_SIGHAND);
        assert_eq!(
            outcome(libc::syscall(libc::SYS_clone, flags, 0, 0, 0, 0)),
            Err(libc::EINVAL)
        );
        // Landlock, which each media tool's child confines itself with.
        assert!(
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                1u32
            ) > 0
        );
        assert_eq!(get(libc::PR_GET_DUMPABLE), 1);
    }
    std::thread::spawn(|| ()).join().unwrap();
    assert!(Command::new("true").status().unwrap().success());
}

/// A socket of `domain`, `kind` and `protocol`, closed again: whether
/// one could be made.
fn socket(domain: libc::c_int, kind: libc::c_int, protocol: libc::c_int) -> Result<(), i32> {
    // SAFETY: socket(2), and close(2) of what it returned.
    unsafe {
        let fd = libc::socket(domain, kind | libc::SOCK_CLOEXEC, protocol);
        if fd < 0 {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap());
        }
        libc::close(fd);
    }
    Ok(())
}

#[test]
fn a_hardened_server_s_own_work_still_runs() {
    if !alone("harden::a_hardened_server_s_own_work_still_runs") {
        return;
    }
    harden::harden().unwrap();
    // As `main` starts it: after the process is hardened.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-harden-work").await;

        // TCP on loopback.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        client.write_all(b"tcp").await.unwrap();
        let mut got = [0; 3];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"tcp");

        // UDP, as QUIC sends.
        let a = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        a.send_to(b"udp", b.local_addr().unwrap()).await.unwrap();
        let mut got = [0; 3];
        b.recv_from(&mut got).await.unwrap();
        assert_eq!(&got, b"udp");
        assert_eq!(socket(libc::AF_INET6, libc::SOCK_DGRAM, 0), Ok(()));
        assert_eq!(socket(libc::AF_INET6, libc::SOCK_STREAM, 0), Ok(()));

        // A Unix socket by path, and a pair.
        let path = tmp.as_ref().join("socket");
        let unix = tokio::net::UnixListener::bind(&path).unwrap();
        let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (mut server, _) = unix.accept().await.unwrap();
        client.write_all(b"unix").await.unwrap();
        let mut got = [0; 4];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"unix");
        tokio::net::UnixStream::pair().unwrap();

        // Netlink routes: how the endpoint watches the interfaces.
        assert_eq!(
            socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE),
            Ok(())
        );

        // SQLite: the store, made and migrated.
        let state = tmp.as_ref().join("state");
        std::fs::create_dir_all(&state).unwrap();
        spoolframe_server::store::Store::open(&state).await.unwrap();

        // A media tool, confined, its filter on top of the server's.
        let sandbox = Sandbox::new(Mode::Required, vec![state.clone()], GpuDevices::default())
            .expect("this kernel has Landlock");
        let shown = Tool::new("ffprobe", sandbox)
            .check_confined(&state)
            .await
            .unwrap();
        assert!(shown.contains("refused reading"), "{shown}");
    });
}
