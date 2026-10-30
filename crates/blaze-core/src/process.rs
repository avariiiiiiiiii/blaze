//! Starting the process of an [`crate::Effect::Spawn`] and making its [`Exit`], for every
//! driver on Unix. A driver starts it here, writes its input, reads its output and learns that
//! it exited in its own way (a thread that blocks, the ring and a pidfd, a kqueue), then reaps
//! it and asks [`Finish::exited`].
//!
//! The process's memory is held by the system where a limit is asked for: a cgroup of its own
//! on Linux, which it joins before it runs its program, and on macOS a resource limit and a
//! watch of its footprint. Where none can be made, it is not started.

use std::{
    io,
    os::{fd::OwnedFd, unix::process::CommandExt as _},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{Command, Confine, Confined, Exit, Sandbox};

static RUNS: AtomicU64 = AtomicU64::new(0);

/// A process started, its pipes the driver's.
pub(crate) struct Started {
    pub(crate) pid: libc::pid_t,
    /// Where its input goes, of a command with input; closing it is the end of the input.
    pub(crate) stdin: Option<OwnedFd>,
    pub(crate) stdout: OwnedFd,
    pub(crate) stderr: OwnedFd,
    pub(crate) finish: Finish,
}

/// What is needed to say how the process ended, once it has.
pub(crate) struct Finish {
    pub(crate) command: Box<Command>,
    confine: Option<Arc<dyn Confine>>,
    scratch: Option<PathBuf>,
    limit: Limit,
}

enum Limit {
    None,
    #[cfg(target_os = "linux")]
    Cgroup(crate::cgroup::Cgroup),
    #[cfg(target_os = "macos")]
    Footprint(crate::footprint::Watch),
}

/// `command` started: in its sandbox, made by `confine`, unless the sandbox is off; with its
/// memory held where a limit is asked for. Its input is in `finish.command.stdin`, for the
/// driver to write to `stdin`.
pub(crate) fn start(
    root: &Path,
    command: Box<Command>,
    confine: Option<&Arc<dyn Confine>>,
) -> io::Result<Started> {
    let n = RUNS.fetch_add(1, Ordering::Relaxed);
    let (confined, scratch, confine) = match &command.sandbox {
        Sandbox::Off => (
            Confined {
                program: command.program.clone(),
                args: command.args.clone(),
                env: command.env.clone(),
                dir: command.dir.clone(),
            },
            None,
            None,
        ),
        Sandbox::On { .. } => {
            let Some(confine) = confine else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "{}: a sandbox is asked for and this driver was given nothing to make \
                         one (no Confine); nothing runs outside one",
                        command.program.display()
                    ),
                ));
            };
            let scratch = root.join(".blaze-spawn").join(format!("{}-{n}", std::process::id()));
            _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch)?;
            match confine.confine(&command, &scratch) {
                Ok(confined) => (confined, Some(scratch), Some(confine.clone())),
                Err(why) => {
                    _ = std::fs::remove_dir_all(&scratch);
                    return Err(io::Error::other(format!(
                        "{}: no sandbox can be made here: {why}",
                        command.program.display()
                    )));
                }
            }
        }
    };
    let removing = |error: io::Error| {
        if let Some(scratch) = &scratch {
            _ = std::fs::remove_dir_all(scratch);
        }
        error
    };
    let mut process = std::process::Command::new(&confined.program);
    process
        .args(&confined.args)
        .env_clear()
        .envs(confined.env.iter().map(|(name, value)| (name, value)))
        .current_dir(&confined.dir)
        .stdin(if command.stdin.is_empty() { Stdio::null() } else { Stdio::piped() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "linux")]
    let cgroup = match command.memory {
        Some(bytes) => {
            let cgroup = crate::cgroup::Cgroup::new(bytes).map_err(|error| {
                removing(io::Error::new(
                    error.kind(),
                    format!(
                        "{}: no cgroup can hold its memory to {bytes} bytes: {error}",
                        command.program.display()
                    ),
                ))
            })?;
            let procs = cgroup.procs().to_owned();
            // SAFETY: the closure makes three system calls and allocates nothing, as a child
            // between fork and exec must.
            unsafe { process.pre_exec(move || join(&procs)) };
            Some(cgroup)
        }
        None => None,
    };
    #[cfg(target_os = "macos")]
    if let Some(bytes) = command.memory {
        // SAFETY: `limit_self` makes system calls alone, as a child between fork and exec must.
        unsafe { process.pre_exec(move || crate::footprint::limit_self(bytes)) };
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    if let Some(bytes) = command.memory {
        return Err(removing(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{}: no limit of {bytes} bytes can be held on this system",
                command.program.display()
            ),
        )));
    }
    let mut child = process.spawn().map_err(|error| {
        removing(io::Error::new(
            error.kind(),
            format!("starting {}: {error}", confined.program.display()),
        ))
    })?;
    let pid = child.id() as libc::pid_t;
    let stdin = child.stdin.take().map(OwnedFd::from);
    let stdout = OwnedFd::from(child.stdout.take().expect("a piped stdout"));
    let stderr = OwnedFd::from(child.stderr.take().expect("a piped stderr"));
    // Dropping a `Child` neither waits for nor kills it: the driver reaps it.
    drop(child);
    #[cfg(target_os = "linux")]
    let limit = cgroup.map_or(Limit::None, Limit::Cgroup);
    #[cfg(target_os = "macos")]
    let limit = match command.memory {
        Some(bytes) => Limit::Footprint(crate::footprint::Watch::start(pid, bytes)),
        None => Limit::None,
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let limit = Limit::None;
    Ok(Started { pid, stdin, stdout, stderr, finish: Finish { command, confine, scratch, limit } })
}

/// In the child before its program runs: into the cgroup whose `cgroup.procs` is `procs`.
#[cfg(target_os = "linux")]
fn join(procs: &std::ffi::CStr) -> io::Result<()> {
    // SAFETY: a NUL-terminated path; the descriptor is closed below.
    let fd = unsafe { libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: one byte from a static, to an open descriptor.
    let wrote = unsafe { libc::write(fd, b"0".as_ptr().cast(), 1) };
    let error = io::Error::last_os_error();
    // SAFETY: the descriptor opened above.
    unsafe { libc::close(fd) };
    match wrote {
        1 => Ok(()),
        _ => Err(error),
    }
}

impl Finish {
    /// Reaps `pid`, which has exited (a zombie, so its pid is no one else's), and says how it
    /// ended from what `wait4` said of it and what it wrote. Asks its sandbox what it read,
    /// and removes what was made for it.
    pub(crate) fn exited(
        self,
        pid: libc::pid_t,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> io::Result<Exit> {
        // Before the reap: the footprint watch signals the pid, which a reap frees for reuse.
        let limited = match self.limit {
            Limit::None => Ok(0),
            #[cfg(target_os = "linux")]
            Limit::Cgroup(cgroup) => cgroup.peak(),
            #[cfg(target_os = "macos")]
            Limit::Footprint(watch) => Ok(watch.stop()),
        };
        let reaped = reap(pid);
        let ((status, rusage), limited) = match (reaped, limited) {
            (Ok(reaped), Ok(limited)) => (reaped, limited),
            (Err(error), _) | (_, Err(error)) => {
                if let Some(scratch) = &self.scratch {
                    _ = std::fs::remove_dir_all(scratch);
                }
                return Err(error);
            }
        };
        let (code, signal) = match libc::WIFSIGNALED(status) {
            true => (None, Some(libc::WTERMSIG(status))),
            false => (Some(libc::WEXITSTATUS(status)), None),
        };
        let max_rss = u64::try_from(rusage.ru_maxrss).unwrap_or(0);
        // Kibibytes on Linux, bytes on Apple's systems.
        let peak_memory =
            if cfg!(target_os = "macos") { max_rss } else { max_rss * 1024 }.max(limited);
        let traced = match (&self.command.sandbox, &self.confine, &self.scratch) {
            (Sandbox::On { trace: true, .. }, Some(confine), Some(scratch)) => {
                Some(confine.traced(&self.command, scratch).map_err(|why| {
                    io::Error::other(format!(
                        "{}: its sandbox's record of what it read: {why}",
                        self.command.program.display()
                    ))
                }))
            }
            _ => None,
        };
        if let Some(scratch) = &self.scratch {
            _ = std::fs::remove_dir_all(scratch);
        }
        let traced = traced.transpose()?;
        Ok(Exit { code, signal, stdout, stderr, traced, peak_memory })
    }
}

/// Reaps `pid`, waiting until it has exited: its status and what it used.
pub(crate) fn reap(pid: libc::pid_t) -> io::Result<(libc::c_int, libc::rusage)> {
    let mut status = 0;
    // SAFETY: all zeros is a valid `rusage`.
    let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: a child of this process; the status and usage are written by the call.
        let reaped = unsafe { libc::wait4(pid, &mut status, 0, &mut rusage) };
        if reaped == pid {
            return Ok((status, rusage));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Waits until `pid` has exited without reaping it, so its pid is not another's until it is.
pub(crate) fn wait_exited(pid: libc::pid_t) -> io::Result<()> {
    loop {
        // SAFETY: all zeros is a valid `siginfo_t`; the call writes it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: a child of this process and a `siginfo_t` to write.
        let waited = unsafe {
            libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        if waited == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Signals `pid`, which has not been reaped.
pub(crate) fn kill(pid: libc::pid_t) {
    // SAFETY: no pointers; the pid is a child not yet reaped, so no one else's.
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

/// A pidfd of `pid`, a child not yet reaped: readable once it exits.
#[cfg(target_os = "linux")]
pub(crate) fn pidfd(pid: libc::pid_t) -> io::Result<OwnedFd> {
    use std::os::fd::FromRawFd as _;
    // SAFETY: no pointers; a descriptor of its own or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call returned a descriptor of its own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}
