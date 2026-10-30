//! The memory backstop of a spawned process on macOS (docs/v2/core.md section 4, "Hard
//! backstop"): resource limits set in the child before its program runs, and a thread that
//! watches its physical footprint, the number Activity Monitor and the kernel's memory pressure
//! go by, and kills it once it passes the limit.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

/// How often the footprint is read: a process faulting pages in as fast as it can gets tens of
/// megabytes past its limit at most before it is killed.
const INTERVAL: Duration = Duration::from_millis(2);

/// In the child between fork and exec (system calls only, no allocation): resource limits
/// for `limit` bytes (RLIMIT_DATA and RLIMIT_AS as macOS honours them).
pub(crate) fn limit_self(limit: u64) -> io::Result<()> {
    for resource in [libc::RLIMIT_DATA, libc::RLIMIT_AS] {
        // SAFETY: all zeros is a valid `rlimit`; the call writes it.
        let mut now: libc::rlimit = unsafe { std::mem::zeroed() };
        // SAFETY: an `rlimit` to write.
        if unsafe { libc::getrlimit(resource, &mut now) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let most = limit.min(now.rlim_max);
        let wanted = libc::rlimit { rlim_cur: most, rlim_max: most };
        // SAFETY: an `rlimit` to read.
        if unsafe { libc::setrlimit(resource, &wanted) } == 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        // macOS refuses a limit below the address space the process has reserved already,
        // which is hundreds of gigabytes of unbacked ranges in every process: that limit is
        // not one macOS holds, and the footprint watch holds it instead.
        if error.raw_os_error() != Some(libc::EINVAL) {
            return Err(error);
        }
    }
    Ok(())
}

struct Shared {
    stop: AtomicBool,
    /// The most footprint read so far.
    seen: AtomicU64,
}

pub(crate) struct Watch {
    pid: libc::pid_t,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Watch {
    /// Watches `pid`'s physical footprint (proc_pid_rusage RUSAGE_INFO_V4 ri_phys_footprint)
    /// on a thread of its own, every few milliseconds, and SIGKILLs it once it passes `limit`.
    pub(crate) fn start(pid: libc::pid_t, limit: u64) -> Watch {
        let shared = Arc::new(Shared { stop: AtomicBool::new(false), seen: AtomicU64::new(0) });
        let thread = std::thread::Builder::new()
            .name(format!("blaze-footprint-{pid}"))
            .spawn({
                let shared = shared.clone();
                move || watch(pid, limit, &shared)
            })
            .expect("starting a footprint watch");
        Watch { pid, shared, thread: Some(thread) }
    }

    /// Called once the process exited and before it is reaped: stops the thread and says
    /// the most it saw (ri_lifetime_max_phys_footprint where it could read it).
    pub(crate) fn stop(mut self) -> u64 {
        self.halt();
        let seen = self.shared.seen.load(Ordering::Acquire);
        match footprint(self.pid) {
            Ok(usage) => seen.max(usage.now).max(usage.most),
            Err(_) => seen,
        }
    }

    /// The thread stopped: it signals nothing once this returns.
    fn halt(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            _ = thread.join();
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.halt();
    }
}

fn watch(pid: libc::pid_t, limit: u64, shared: &Shared) {
    while !shared.stop.load(Ordering::Acquire) {
        match footprint(pid) {
            Ok(usage) => {
                shared.seen.fetch_max(usage.now.max(usage.most), Ordering::AcqRel);
                if usage.now > limit {
                    // SAFETY: no pointers; the pid is not reaped until this thread has stopped.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    return;
                }
            }
            // Gone: there is nothing left to hold.
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return,
            // Its footprint cannot be read, so its limit cannot be held: it does not run on.
            Err(_) => {
                // SAFETY: as above.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                return;
            }
        }
        std::thread::park_timeout(INTERVAL);
    }
}

struct Usage {
    now: u64,
    most: u64,
}

fn footprint(pid: libc::pid_t) -> io::Result<Usage> {
    // SAFETY: all zeros is a valid `rusage_info_v4`; the call writes it.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: a `rusage_info_v4`, the size the flavor names, to write.
    let read = unsafe {
        libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut info).cast::<libc::rusage_info_t>())
    };
    if read != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Usage { now: info.ri_phys_footprint, most: info.ri_lifetime_max_phys_footprint })
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt as _;

    use super::*;

    fn spawn(script: &str, limit: Option<u64>) -> std::process::Child {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]).env_clear().stdout(std::process::Stdio::piped());
        if let Some(limit) = limit {
            // SAFETY: `limit_self` makes system calls alone.
            unsafe { command.pre_exec(move || limit_self(limit)) };
        }
        command.spawn().unwrap()
    }

    /// Waits for it to exit, stops the watch, then reaps it, as a driver does.
    fn end(child: std::process::Child, watch: Watch) -> (libc::c_int, u64) {
        let pid = child.id() as libc::pid_t;
        drop(child);
        crate::process::wait_exited(pid).unwrap();
        let peak = watch.stop();
        let (status, _) = crate::process::reap(pid).unwrap();
        (status, peak)
    }

    #[test]
    fn a_process_past_its_limit_is_killed() {
        let limit = 32 << 20;
        // Doubles a string until it is a gigabyte, unless it is stopped first.
        let child = spawn(
            r#"x=a; i=0; while [ $i -lt 30 ]; do x="$x$x"; i=$((i+1)); done; exit 0"#,
            None,
        );
        let watch = Watch::start(child.id() as libc::pid_t, limit);
        let (status, peak) = end(child, watch);
        assert!(libc::WIFSIGNALED(status), "it ran to its end: {status:#x}");
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
        assert!(peak > limit, "{peak}");
    }

    #[test]
    fn a_process_within_its_limit_runs_to_its_end() {
        let child = spawn("exit 3", None);
        let watch = Watch::start(child.id() as libc::pid_t, 1 << 30);
        let (status, peak) = end(child, watch);
        assert!(!libc::WIFSIGNALED(status));
        assert_eq!(libc::WEXITSTATUS(status), 3);
        assert!(peak > 0 && peak < 1 << 30, "{peak}");
    }

    /// The resource limits leave a program that fits them as it is.
    #[test]
    fn limits_set_before_exec_let_a_small_program_run() {
        let child = spawn("echo hi", Some(1 << 30));
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"hi\n");
    }
}
