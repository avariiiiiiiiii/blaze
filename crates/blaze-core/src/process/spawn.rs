//! Starting a process from what the parent prepared in full, so nothing in the child allocates
//! or locks. On Linux a `clone` that shares the parent's memory and stops the calling thread
//! until the child runs its program (`CLONE_VM | CLONE_VFORK`, as `posix_spawn` does), so a
//! spawn costs the same however large the warm daemon is, where a `fork` copies its page
//! tables; the child joins its cgroup before its program runs, and the pidfd comes back with
//! the pid. On macOS `posix_spawn`. Elsewhere std's spawn.

use std::{io, os::fd::OwnedFd};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, OsStr, OsString},
    os::{
        fd::{AsRawFd as _, FromRawFd as _, RawFd},
        unix::{ffi::OsStrExt as _, fs::PermissionsExt as _},
    },
    path::{Path, PathBuf},
    ptr,
};

use crate::Confined;

pub(super) struct Spawned {
    pub(super) pid: libc::pid_t,
    #[cfg(target_os = "linux")]
    pub(super) pidfd: OwnedFd,
    pub(super) stdin: Option<OwnedFd>,
    pub(super) stdout: OwnedFd,
    pub(super) stderr: OwnedFd,
}

/// `confined` started with its output piped, its input piped where it has `input` (else
/// `/dev/null`), joining the cgroup whose `cgroup.procs` is `procs` before its program runs.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn spawn(confined: &Confined, input: bool, procs: Option<&CStr>) -> io::Result<Spawned> {
    let plan = Plan::new(confined)?;
    let (stdin_child, stdin) = match input {
        true => {
            let (read, write) = pipe()?;
            (read, Some(write))
        }
        false => (open(c"/dev/null", libc::O_RDONLY)?, None),
    };
    let (stdout, stdout_child) = pipe()?;
    let (stderr, stderr_child) = pipe()?;
    let fds = [stdin_child.as_raw_fd(), stdout_child.as_raw_fd(), stderr_child.as_raw_fd()];
    // The child's ends close when this returns: by then the child has its own copies.
    #[cfg(target_os = "linux")]
    {
        let procs = procs.map(|path| open(path, libc::O_WRONLY)).transpose()?;
        let (pid, pidfd) = clone(&plan, fds, procs.as_ref().map_or(-1, |it| it.as_raw_fd()))?;
        Ok(Spawned { pid, pidfd, stdin, stdout, stderr })
    }
    #[cfg(target_os = "macos")]
    {
        debug_assert!(procs.is_none(), "no cgroups on macOS");
        let pid = posix_spawn(&plan, fds)?;
        Ok(Spawned { pid, stdin, stdout, stderr })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn spawn(
    confined: &Confined,
    input: bool,
    procs: Option<&std::ffi::CStr>,
) -> io::Result<Spawned> {
    use std::process::Stdio;
    debug_assert!(procs.is_none(), "no cgroups here");
    let mut child = std::process::Command::new(&confined.program)
        .args(&confined.args)
        .env_clear()
        .envs(confined.env.iter().map(|(name, value)| (name, value)))
        .current_dir(&confined.dir)
        .stdin(if input { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id() as libc::pid_t;
    let stdin = child.stdin.take().map(OwnedFd::from);
    let stdout = OwnedFd::from(child.stdout.take().expect("a piped stdout"));
    let stderr = OwnedFd::from(child.stderr.take().expect("a piped stderr"));
    // Dropping a `Child` neither waits for nor kills it: the driver reaps it.
    drop(child);
    Ok(Spawned { pid, stdin, stdout, stderr })
}

/// The program, its arguments, environment and directory as the system calls take them.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Plan {
    program: CString,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
    dir: CString,
    /// What `argv` and `envp` point into: a `CString`'s bytes stay put when it moves.
    _strings: Vec<CString>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Plan {
    /// As std's spawn makes them: `argv[0]` the program as named, the environment with a later
    /// value of a name replacing an earlier one, in order of name.
    fn new(confined: &Confined) -> io::Result<Plan> {
        let c = |bytes: &[u8], what: &str| {
            CString::new(bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{}: {what} has a NUL byte", confined.program.display()),
                )
            })
        };
        let program = c(resolve(&confined.program, &confined.env)?.as_os_str().as_bytes(), "its path")?;
        let env: BTreeMap<&OsStr, &OsStr> =
            confined.env.iter().map(|(name, value)| (name.as_os_str(), value.as_os_str())).collect();
        let mut strings = Vec::with_capacity(1 + confined.args.len() + env.len());
        strings.push(c(confined.program.as_os_str().as_bytes(), "its name")?);
        for arg in &confined.args {
            strings.push(c(arg.as_bytes(), "an argument")?);
        }
        let args = strings.len();
        for (name, value) in env {
            let mut pair = Vec::with_capacity(name.len() + 1 + value.len());
            pair.extend_from_slice(name.as_bytes());
            pair.push(b'=');
            pair.extend_from_slice(value.as_bytes());
            strings.push(c(&pair, "its environment")?);
        }
        let pointers = |it: &[CString]| it.iter().map(|it| it.as_ptr()).chain([ptr::null()]).collect();
        Ok(Plan {
            program,
            argv: pointers(&strings[..args]),
            envp: pointers(&strings[args..]),
            dir: c(confined.dir.as_os_str().as_bytes(), "its directory")?,
            _strings: strings,
        })
    }
}

/// The file `program` names: itself where it has a `/`, else the first executable file of that
/// name on the `PATH` of its environment, or of the daemon's where it sets none, as std's spawn
/// looks it up.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn resolve(program: &Path, env: &[(OsString, OsString)]) -> io::Result<PathBuf> {
    if program.as_os_str().as_bytes().contains(&b'/') {
        return Ok(program.to_owned());
    }
    let path = env
        .iter()
        .rev()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let dir = if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir };
        let candidate = dir.join(program);
        let runs = std::fs::metadata(&candidate)
            .is_ok_and(|it| it.is_file() && it.permissions().mode() & 0o111 != 0);
        if runs {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} is in no directory of its PATH", program.display()),
    ))
}

/// A pipe, its read end first, both closed on exec and numbered past 2.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0; 2];
    // SAFETY: two descriptors written by the call.
    #[cfg(target_os = "linux")]
    let made = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) };
    // SAFETY: as above; macOS has no `pipe2`, and its spawns close what they are not given.
    #[cfg(target_os = "macos")]
    let made = unsafe { libc::pipe(ends.as_mut_ptr()) };
    if made != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call made both; each is owned once from here.
    let read = unsafe { OwnedFd::from_raw_fd(ends[0]) };
    // SAFETY: as above.
    let write = unsafe { OwnedFd::from_raw_fd(ends[1]) };
    Ok((above_stdio(read)?, above_stdio(write)?))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open(path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: a NUL-terminated path.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(error.kind(), format!("{}: {error}", path.to_string_lossy())));
    }
    // SAFETY: the call returned a descriptor of its own.
    above_stdio(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `fd` closed on exec and numbered past 2, so the child's `dup2`s onto 0, 1 and 2 never land
/// on a descriptor another of them still needs (a daemon started with its standard ones closed).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > 2 {
        #[cfg(target_os = "macos")]
        crate::net::set_cloexec(fd.as_raw_fd())?;
        return Ok(fd);
    }
    // SAFETY: an open descriptor; the copy is a descriptor of its own.
    let moved = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if moved < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    Ok(unsafe { OwnedFd::from_raw_fd(moved) })
}

/// The child's stack: what `run` and the calls it makes need, with room to spare.
#[cfg(target_os = "linux")]
const STACK: usize = 64 << 10;

/// What the child reads, and where it says what failed.
#[cfg(target_os = "linux")]
struct Child<'a> {
    plan: &'a Plan,
    fds: [RawFd; 3],
    /// `cgroup.procs` of its cgroup, or -1.
    procs: RawFd,
    /// The step that failed (0: none) and its errno.
    step: u8,
    failed: libc::c_int,
}

#[cfg(target_os = "linux")]
fn clone(plan: &Plan, fds: [RawFd; 3], procs: RawFd) -> io::Result<(libc::pid_t, OwnedFd)> {
    let mut stack = vec![0u8; STACK];
    // It grows down from its end, which both ABIs (x86-64, AArch64) want on 16 bytes.
    let top = stack.as_mut_ptr().wrapping_add(STACK).map_addr(|it| it & !15);
    let mut child = Child { plan, fds, procs, step: 0, failed: 0 };
    let mut pidfd: libc::c_int = -1;
    // SAFETY: all zeros is a valid `sigset_t`; the calls below fill it.
    let mut all: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let mut was: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: a `sigset_t` to fill.
    unsafe { libc::sigfillset(&mut all) };
    // Until the child has reset its handlers, a signal would run one of the daemon's on the
    // daemon's memory from the child.
    // SAFETY: two `sigset_t`s, one to read and one to write.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &all, &mut was) };
    // SAFETY: `run` takes the `Child` it is given and touches nothing else of the parent's.
    // With CLONE_VM the child runs on `stack` in this process's memory, and with CLONE_VFORK
    // this thread waits until the child has run its program or exited, so `child`, `plan` and
    // `stack` outlive every use of them in the child. CLONE_PIDFD writes the pidfd to `pidfd`.
    let pid = unsafe {
        libc::clone(
            run,
            top.cast(),
            libc::CLONE_VM | libc::CLONE_VFORK | libc::CLONE_PIDFD | libc::SIGCHLD,
            (&raw mut child).cast(),
            &raw mut pidfd,
        )
    };
    let error = io::Error::last_os_error();
    // SAFETY: the mask read above.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &was, ptr::null_mut()) };
    if pid < 0 {
        return Err(error);
    }
    // SAFETY: the kernel made it for this call.
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
    // SAFETY: written by the child, which has run its program or exited; volatile because the
    // compiler cannot see a write made through the pointer given to `clone`.
    let step = unsafe { ptr::read_volatile(&raw const child.step) };
    // SAFETY: as above.
    let failed = unsafe { ptr::read_volatile(&raw const child.failed) };
    drop(stack);
    if step == 0 {
        return Ok((pid, pidfd));
    }
    _ = super::reap(pid);
    let what = match step {
        1 => "giving it its input and output",
        2 => "putting it in its cgroup",
        3 => "changing to its directory",
        _ => "running its program",
    };
    let error = io::Error::from_raw_os_error(failed);
    Err(io::Error::new(error.kind(), format!("{what}: {error}")))
}

/// The child, between `clone` and its program: system calls alone, nothing that allocates,
/// locks or could panic. It writes only `Child`, through which the parent hears what failed.
#[cfg(target_os = "linux")]
extern "C" fn run(arg: *mut libc::c_void) -> libc::c_int {
    // SAFETY: the `Child` `clone` was given; the thread that made it waits (CLONE_VFORK) and
    // nothing else has it.
    let child = unsafe { &mut *arg.cast::<Child<'_>>() };
    let fail = |child: &mut Child<'_>, step: u8| -> ! {
        // SAFETY: the calling thread's errno, in the TLS the child shares with the waiting
        // parent thread.
        let errno = unsafe { libc::__errno_location() };
        // SAFETY: as above; nothing else writes it until the parent runs again.
        child.failed = unsafe { *errno };
        child.step = step;
        // SAFETY: the end of the child, running nothing of the parent's.
        unsafe { libc::_exit(127) }
    };
    for signal in 1..65 {
        // SAFETY: all zeros is a valid `sigaction`; the call writes it.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: a `sigaction` to write; a signal the C library keeps for itself fails.
        if unsafe { libc::sigaction(signal, ptr::null(), &mut old) } != 0 {
            continue;
        }
        // Ignored stays ignored across exec, but SIGPIPE, which std ignores in the daemon and
        // gives back to its children.
        if old.sa_sigaction == libc::SIG_IGN && signal != libc::SIGPIPE {
            continue;
        }
        // SAFETY: as above.
        let mut default: libc::sigaction = unsafe { std::mem::zeroed() };
        default.sa_sigaction = libc::SIG_DFL;
        // SAFETY: a `sigaction` to read.
        unsafe { libc::sigaction(signal, &default, ptr::null_mut()) };
    }
    // SAFETY: all zeros is a valid `sigset_t`.
    let mut none: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: a `sigset_t` to empty.
    unsafe { libc::sigemptyset(&mut none) };
    // SAFETY: a `sigset_t` to read.
    unsafe { libc::sigprocmask(libc::SIG_SETMASK, &none, ptr::null_mut()) };
    for (target, fd) in (0..).zip(child.fds) {
        // SAFETY: an open descriptor past 2 onto one of 0, 1 and 2; the copy keeps no
        // close-on-exec.
        if unsafe { libc::dup2(fd, target) } < 0 {
            fail(child, 1);
        }
    }
    // SAFETY: one byte from a static, to an open descriptor.
    if child.procs >= 0 && unsafe { libc::write(child.procs, b"0".as_ptr().cast(), 1) } != 1 {
        fail(child, 2);
    }
    // SAFETY: a NUL-terminated path the parent keeps.
    if unsafe { libc::chdir(child.plan.dir.as_ptr()) } != 0 {
        fail(child, 3);
    }
    // SAFETY: a NUL-terminated path and two NULL-terminated arrays of NUL-terminated strings,
    // all kept by the parent until the program runs.
    unsafe {
        libc::execve(child.plan.program.as_ptr(), child.plan.argv.as_ptr(), child.plan.envp.as_ptr())
    };
    fail(child, 4)
}

#[cfg(target_os = "macos")]
fn posix_spawn(plan: &Plan, fds: [RawFd; 3]) -> io::Result<libc::pid_t> {
    unsafe extern "C" {
        fn posix_spawn_file_actions_addchdir_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            path: *const libc::c_char,
        ) -> libc::c_int;
    }
    /// Every descriptor not given by a file action is closed in the child (Apple's).
    const CLOEXEC_DEFAULT: libc::c_short = 0x4000;
    let check = |code: libc::c_int| match code {
        0 => Ok(()),
        _ => Err(io::Error::from_raw_os_error(code)),
    };
    let mut actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
    // SAFETY: a `posix_spawn_file_actions_t` to make.
    check(unsafe { libc::posix_spawn_file_actions_init(&mut actions) })?;
    let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
    // SAFETY: a `posix_spawnattr_t` to make.
    if let Err(error) = check(unsafe { libc::posix_spawnattr_init(&mut attr) }) {
        // SAFETY: made above.
        unsafe { libc::posix_spawn_file_actions_destroy(&mut actions) };
        return Err(error);
    }
    let spawned = (|| {
        for (target, fd) in (0..).zip(fds) {
            // SAFETY: made above; an open descriptor onto one of 0, 1 and 2.
            check(unsafe { libc::posix_spawn_file_actions_adddup2(&mut actions, fd, target) })?;
        }
        // SAFETY: made above; a NUL-terminated path kept until the spawn.
        check(unsafe { posix_spawn_file_actions_addchdir_np(&mut actions, plan.dir.as_ptr()) })?;
        // SIGPIPE, which std ignores in the daemon, back to its default in the child, and the
        // child's mask empty, as std's spawn makes them. On macOS `sigset_t` is an integer.
        let mut defaults: libc::sigset_t = 0;
        // SAFETY: a `sigset_t` to fill.
        unsafe { libc::sigaddset(&mut defaults, libc::SIGPIPE) };
        // SAFETY: made above; a `sigset_t` to read.
        check(unsafe { libc::posix_spawnattr_setsigdefault(&mut attr, &defaults) })?;
        let none: libc::sigset_t = 0;
        // SAFETY: as above.
        check(unsafe { libc::posix_spawnattr_setsigmask(&mut attr, &none) })?;
        let flags = CLOEXEC_DEFAULT
            | libc::POSIX_SPAWN_SETSIGDEF as libc::c_short
            | libc::POSIX_SPAWN_SETSIGMASK as libc::c_short;
        // SAFETY: made above.
        check(unsafe { libc::posix_spawnattr_setflags(&mut attr, flags) })?;
        let mut pid = 0;
        // SAFETY: a NUL-terminated path, the actions and attributes made above, and two
        // NULL-terminated arrays of NUL-terminated strings kept by `plan`.
        check(unsafe {
            libc::posix_spawn(
                &mut pid,
                plan.program.as_ptr(),
                &actions,
                &attr,
                plan.argv.as_ptr().cast(),
                plan.envp.as_ptr().cast(),
            )
        })?;
        Ok(pid)
    })();
    // SAFETY: made above, destroyed once.
    unsafe { libc::posix_spawnattr_destroy(&mut attr) };
    // SAFETY: as above.
    unsafe { libc::posix_spawn_file_actions_destroy(&mut actions) };
    spawned
}
