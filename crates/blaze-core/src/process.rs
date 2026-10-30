//! Starting the process of an [`crate::Effect::Spawn`] and making its [`Exit`], for every
//! driver on Unix. A driver starts it here, writes its input, reads its output and learns that
//! it exited in its own way (a thread that blocks, the ring and a pidfd, a kqueue), then reaps
//! it and asks [`Finish::exited`].
//!
//! The process's memory is held by the system where a limit is asked for: a cgroup of its own
//! on Linux, which it joins before it runs its program, and on macOS a watch of its footprint.
//! Where none can be made, it is not started.

mod spawn;

use std::{
    io,
    os::fd::OwnedFd,
    path::{Path, PathBuf},
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
    /// Readable once it exits; made with it, so it names this process and no later one.
    #[cfg(target_os = "linux")]
    pub(crate) pidfd: OwnedFd,
    /// Where its input goes, of a command with input; closing it is the end of the input.
    pub(crate) stdin: Option<OwnedFd>,
    pub(crate) stdout: OwnedFd,
    pub(crate) stderr: OwnedFd,
    pub(crate) finish: Finish,
}

/// What is needed to say how the process ended, once it has.
pub(crate) struct Finish {
    pub(crate) command: Box<Command>,
    // dyn: the platform's sandbox, given by whoever owns it; asked once per spawn.
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
    // dyn: as `Finish::confine`.
    confine: Option<&Arc<dyn Confine>>,
) -> io::Result<Started> {
    let n = RUNS.fetch_add(1, Ordering::Relaxed);
    let (confined, scratch, confine): (Option<Confined>, _, _) = match &command.sandbox {
        Sandbox::Off => (None, None, None),
        Sandbox::On { .. } => {
            let Some(confine) = confine else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    // alloc: an error path.
                    format!(
                        "{}: a sandbox is asked for and this driver was given nothing to make \
                         one (no Confine); nothing runs outside one",
                        command.program.display()
                    ),
                ));
            };
            // alloc: once per sandboxed spawn, its scratch directory, kept until it exits.
            let scratch = root.join(".blaze-spawn").join(format!("{}-{n}", std::process::id()));
            _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch)?;
            match confine.confine(&command, &scratch) {
                // clone: the sandbox maker, kept to ask what the process read.
                Ok(confined) => (Some(confined), Some(scratch), Some(confine.clone())),
                Err(why) => {
                    _ = std::fs::remove_dir_all(&scratch);
                    // alloc: an error path.
                    return Err(io::Error::other(format!(
                        "{}: no sandbox can be made here: {why}",
                        command.program.display()
                    )));
                }
            }
        }
    };
    // What the sandbox starts, else the command itself, borrowed either way.
    let program = match &confined {
        Some(it) => spawn::Program { program: &it.program, args: &it.args, env: &it.env, dir: &it.dir },
        None => spawn::Program {
            program: &command.program,
            args: &command.args,
            env: &command.env,
            dir: &command.dir,
        },
    };
    let removing = |error: io::Error| {
        if let Some(scratch) = &scratch {
            _ = std::fs::remove_dir_all(scratch);
        }
        error
    };
    #[cfg(target_os = "linux")]
    let cgroup = match command.memory {
        Some(bytes) => Some(crate::cgroup::Cgroup::new(bytes).map_err(|error| {
            removing(io::Error::new(
                error.kind(),
                // alloc: an error path.
                format!(
                    "{}: no cgroup can hold its memory to {bytes} bytes: {error}",
                    command.program.display()
                ),
            ))
        })?),
        None => None,
    };
    #[cfg(target_os = "linux")]
    let procs = cgroup.as_ref().map(crate::cgroup::Cgroup::procs);
    #[cfg(not(target_os = "linux"))]
    let procs = None;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    if let Some(bytes) = command.memory {
        return Err(removing(io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: an error path.
            format!(
                "{}: no limit of {bytes} bytes can be held on this system",
                command.program.display()
            ),
        )));
    }
    let spawned = spawn::spawn(&program, !command.stdin.is_empty(), procs).map_err(|error| {
        removing(io::Error::new(
            error.kind(),
            // alloc: an error path.
            format!("starting {}: {error}", program.program.display()),
        ))
    })?;
    let pid = spawned.pid;
    #[cfg(target_os = "linux")]
    let limit = cgroup.map_or(Limit::None, Limit::Cgroup);
    #[cfg(target_os = "macos")]
    let limit = match command.memory {
        Some(bytes) => Limit::Footprint(crate::footprint::Watch::start(pid, bytes)),
        None => Limit::None,
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let limit = Limit::None;
    Ok(Started {
        pid,
        #[cfg(target_os = "linux")]
        pidfd: spawned.pidfd,
        stdin: spawned.stdin,
        stdout: spawned.stdout,
        stderr: spawned.stderr,
        finish: Finish { command, confine, scratch, limit },
    })
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
                    // alloc: an error path.
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

/// What one read of a pipe takes at most.
const CHUNK: usize = 16 << 10;

/// Writes `input` to `stdin` and reads `stdout` and `stderr` until both end, on this thread in
/// one poll loop: what a driver that blocks does in place of a thread per pipe. Returns what
/// each said. A process that ends without reading all of its input, or a pipe that fails, is
/// not an error of the driver's: what was read is what it said, and its end says how it went.
/// Fails only where the loop cannot wait, and then the process is the caller's to kill.
pub(crate) fn communicate(
    stdin: Option<OwnedFd>,
    input: &[u8],
    stdout: OwnedFd,
    stderr: OwnedFd,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    use std::os::fd::AsRawFd as _;
    let mut stdin = stdin.filter(|_| !input.is_empty());
    if let Some(fd) = &stdin {
        // SAFETY: an open descriptor of the caller's.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: as above.
        if flags < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let mut written = 0;
    let mut outputs = [Some(stdout), Some(stderr)];
    // alloc: none until the process writes; what it writes is the exit's payload.
    let mut said: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
    let mut chunk = [0u8; CHUNK];
    loop {
        // Which pipe each pollfd is: the input, or an output by its index.
        let mut fds = [libc::pollfd { fd: -1, events: 0, revents: 0 }; 3];
        let mut which = [None; 3];
        let mut n = 0;
        if let Some(fd) = &stdin {
            fds[n] = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
            n += 1;
        }
        for (at, fd) in outputs.iter().enumerate() {
            if let Some(fd) = fd {
                fds[n] = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                which[n] = Some(at);
                n += 1;
            }
        }
        if n == 0 {
            let [stdout, stderr] = said;
            return Ok((stdout, stderr));
        }
        // SAFETY: `n` pollfds of open descriptors.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), n as libc::nfds_t, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        for (fd, which) in fds[..n].iter().zip(which) {
            if fd.revents == 0 {
                continue;
            }
            match which {
                None => {
                    // SAFETY: the rest of `input`, to an open descriptor.
                    let wrote =
                        unsafe { libc::write(fd.fd, input[written..].as_ptr().cast(), input.len() - written) };
                    let again = wrote < 0
                        && matches!(
                            io::Error::last_os_error().kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        );
                    if wrote > 0 {
                        written += wrote as usize;
                    }
                    // Done, or the process is not reading (EPIPE and the rest): its input ends.
                    if written == input.len() || (wrote < 0 && !again) || fd.revents & libc::POLLOUT == 0 {
                        stdin = None;
                    }
                }
                Some(at) => {
                    // SAFETY: room for `CHUNK` bytes, from an open descriptor.
                    let read = unsafe { libc::read(fd.fd, chunk.as_mut_ptr().cast(), CHUNK) };
                    match read {
                        1.. => said[at].extend_from_slice(&chunk[..read as usize]),
                        0 => outputs[at] = None,
                        _ => match io::Error::last_os_error().kind() {
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => {}
                            _ => outputs[at] = None,
                        },
                    }
                }
            }
        }
    }
}
